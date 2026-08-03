// The Puppeteer conformance harness. Driven by `cargo xtask puppeteer`, which
// starts the endpoint and a loopback fixture server and passes both here.
//
// Output contract: one `PASS\t<name>` or `FAIL\t<name>\t<message>` line per
// check, on stdout, nothing else. The runner compares those to
// `expectations.tsv` with the same two-sided rule as WPT — a regression *and*
// an unexpected pass both fail CI, so fixing something forces the expectation
// edit into the same commit.
//
// `puppeteer-core`, not `puppeteer`: the latter downloads a Chromium at install
// time, and the whole point here is to drive OxidePage.

import puppeteer from 'puppeteer-core';

const args = new Map();
for (let i = 2; i < process.argv.length; i += 2) {
  args.set(process.argv[i].replace(/^--/, ''), process.argv[i + 1]);
}
const endpoint = args.get('endpoint');
const base = args.get('base');
if (!endpoint || !base) {
  console.error('usage: run.mjs --endpoint <ws://…> --base <http://…>');
  process.exit(2);
}

/** Every check that has been run, in order. */
const results = [];

function report(name, error) {
  if (error) {
    const message = String(error && error.message ? error.message : error)
      .replace(/[\r\n\t]+/g, ' ')
      .slice(0, 300);
    results.push(`FAIL\t${name}\t${message}`);
  } else {
    results.push(`PASS\t${name}`);
  }
}

/** Runs one named check, recording its outcome rather than throwing. */
async function check(name, body) {
  try {
    await body();
    report(name);
  } catch (error) {
    report(name, error);
  }
}

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

function assertEqual(actual, expected, what) {
  if (actual !== expected) {
    throw new Error(`${what}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
  }
}

/** A promise that rejects if `inner` has not settled within `ms`. */
function within(ms, what, inner) {
  let timer;
  const guard = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(`timed out after ${ms}ms waiting for ${what}`)), ms);
  });
  return Promise.race([inner, guard]).finally(() => clearTimeout(timer));
}

const browser = await puppeteer.connect({
  browserWSEndpoint: endpoint,
  // The endpoint reports no window size of its own, and letting Puppeteer
  // impose one would make every check depend on `Emulation`.
  defaultViewport: null,
});

try {
  await check('browser.version', async () => {
    const version = await browser.version();
    assert(version.includes('OxidePage'), `unexpected version: ${version}`);
  });

  const page = await browser.newPage();

  await check('page.goto', async () => {
    const response = await page.goto(`${base}/index.html`);
    assert(response === null || response.ok?.() !== false, 'navigation did not succeed');
    assertEqual(page.url(), `${base}/index.html`, 'page.url()');
  });

  await check('page.title', async () => {
    assertEqual(await page.title(), 'OxidePage automation fixture', 'title');
  });

  await check('page.evaluate primitive', async () => {
    assertEqual(await page.evaluate(() => 6 * 7), 42, 'evaluate');
  });

  await check('page.evaluate object', async () => {
    const value = await page.evaluate(() => ({ a: [1, 2], b: 'x' }));
    assertEqual(JSON.stringify(value), JSON.stringify({ a: [1, 2], b: 'x' }), 'evaluate object');
  });

  await check('page.evaluate with arguments', async () => {
    assertEqual(await page.evaluate((a, b) => a + b, 20, 22), 42, 'evaluate args');
  });

  await check('page.evaluate reads the DOM', async () => {
    const text = await page.evaluate(() => document.querySelector('#heading').textContent);
    assertEqual(text, 'Fixture', 'heading text');
  });

  await check('page.evaluate rejects on a thrown error', async () => {
    let threw = false;
    try {
      await page.evaluate(() => {
        throw new TypeError('from the page');
      });
    } catch (error) {
      threw = true;
      assert(String(error).includes('from the page'), `unhelpful rejection: ${error}`);
    }
    assert(threw, 'a throwing evaluate must reject');
  });

  await check('page.evaluateHandle and JSHandle', async () => {
    const handle = await page.evaluateHandle(() => ({ n: 21 }));
    const doubled = await handle.evaluate((o) => o.n * 2);
    assertEqual(doubled, 42, 'handle round trip');
    await handle.dispose();
  });

  await check('page.$ and elementHandle.evaluate', async () => {
    const element = await page.$('#heading');
    assert(element, 'page.$ returned null');
    assertEqual(await element.evaluate((el) => el.id), 'heading', 'element id');
  });

  await check('page.$$', async () => {
    const elements = await page.$$('.para');
    assertEqual(elements.length, 2, 'page.$$ length');
  });

  await check('page.$eval', async () => {
    assertEqual(await page.$eval('#heading', (el) => el.textContent), 'Fixture', '$eval');
  });

  await check('page.waitForSelector', async () => {
    const element = await within(10_000, 'waitForSelector', page.waitForSelector('#box'));
    assert(element, 'waitForSelector returned null');
  });

  await check('page.content', async () => {
    const html = await page.content();
    assert(html.includes('<h1 id="heading">'), 'content did not include the heading');
  });

  await check('page.screenshot', async () => {
    const shot = await page.screenshot();
    assert(shot.length > 0, 'empty screenshot');
    // PNG signature.
    assert(shot[0] === 0x89 && shot[1] === 0x50, 'not a PNG');
  });

  await check('page.pdf', async () => {
    const pdf = await page.pdf();
    assert(pdf.length > 0, 'empty pdf');
    assertEqual(Buffer.from(pdf.slice(0, 4)).toString('latin1'), '%PDF', 'pdf magic');
  });

  await check('page.on console', async () => {
    const seen = new Promise((resolve) => page.once('console', resolve));
    await page.evaluate(() => console.log('hello from the page'));
    const message = await within(10_000, 'console event', seen);
    assert(message.text().includes('hello from the page'), `unexpected text: ${message.text()}`);
  });

  await check('page.on pageerror', async () => {
    const errors = new Promise((resolve) => page.once('pageerror', resolve));
    await page.goto(`${base}/throws.html`);
    const error = await within(10_000, 'pageerror', errors);
    assert(String(error).includes('fixture blew up'), `unexpected error: ${error}`);
  });

  await check('page.goBack', async () => {
    await page.goto(`${base}/index.html`);
    await page.goto(`${base}/other.html`);
    await page.goBack();
    assertEqual(page.url(), `${base}/index.html`, 'url after goBack');
  });

  await check('page.reload', async () => {
    await page.goto(`${base}/index.html`);
    await page.reload();
    assertEqual(await page.title(), 'OxidePage automation fixture', 'title after reload');
  });

  await check('page.setViewport', async () => {
    await page.setViewport({ width: 500, height: 400 });
    const size = await page.evaluate(() => [innerWidth, innerHeight]);
    assertEqual(JSON.stringify(size), JSON.stringify([500, 400]), 'viewport');
  });

  await check('page cookies', async () => {
    await page.setCookie({ name: 'probe', value: 'yes', url: `${base}/` });
    const cookies = await page.cookies(`${base}/`);
    const probe = cookies.find((cookie) => cookie.name === 'probe');
    assert(probe, `cookie not found among ${JSON.stringify(cookies.map((c) => c.name))}`);
    assertEqual(probe.value, 'yes', 'cookie value');
    await page.deleteCookie({ name: 'probe', url: `${base}/` });
    const after = await page.cookies(`${base}/`);
    assert(!after.some((cookie) => cookie.name === 'probe'), 'cookie survived deletion');
  });

  await check('page.exposeFunction', async () => {
    const seen = [];
    await page.exposeFunction('__record', (value) => {
      seen.push(value);
      return value;
    });
    await page.evaluate(() => globalThis.__record('called'));
    // The binding is delivered as a task, so give the loop a turn to run it.
    await new Promise((resolve) => setTimeout(resolve, 300));
    assert(seen.includes('called'), `binding never fired; saw ${JSON.stringify(seen)}`);
  });

  await check('page.click', async () => {
    await page.goto(`${base}/index.html`);
    await page.click('#link');
    assertEqual(page.url(), `${base}/other.html`, 'url after click');
  });

  await check('page.type', async () => {
    await page.goto(`${base}/index.html`);
    await page.click('#field', { clickCount: 3 });
    await page.type('#field', 'typed');
    const value = await page.$eval('#field', (el) => el.value);
    assert(value.includes('typed'), `field value: ${value}`);
  });

  await check('browserContext isolation', async () => {
    const context = await browser.createBrowserContext();
    const isolated = await context.newPage();
    await isolated.goto(`${base}/index.html`);
    assertEqual(await isolated.title(), 'OxidePage automation fixture', 'isolated title');
    await context.close();
  });

  await check('page.close', async () => {
    const extra = await browser.newPage();
    await extra.close();
    assert(extra.isClosed(), 'page did not report itself closed');
  });
} finally {
  // `disconnect`, not `close`: the endpoint is owned by the runner, which stops
  // it when the harness exits. `browser.close()` here would race the runner's
  // own teardown and turn a clean run into a transport error.
  try {
    await browser.disconnect();
  } catch {
    // The socket may already be gone; that is not a check result.
  }
  process.stdout.write(`${results.join('\n')}\n`);
}

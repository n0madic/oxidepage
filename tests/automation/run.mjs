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
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';

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

// A per-run download directory. `Browser.setDownloadBehavior` resolves and
// creates it too, but making it here keeps the check's own `readFile` honest
// about which directory it is asserting on.
const downloadDir = await fs.mkdtemp(path.join(os.tmpdir(), 'oxidepage-automation-'));

/**
 * How long any one check may take before it is failed instead of left to hang
 * the whole harness.
 *
 * Generous by two orders of magnitude — the entire suite runs in about twelve
 * seconds — because the only thing this bound has to catch is a check awaiting
 * something that will never settle. Without it a single protocol hiccup takes
 * the runner's 180 s backstop and reports nothing at all about where it stopped.
 */
const CHECK_TIMEOUT_MS = 30000;

function report(name, error) {
  if (error) {
    const message = String(error && error.message ? error.message : error)
      .replace(/[\r\n\t]+/g, ' ')
      .slice(0, 300);
    // Written as it happens, not collected for the end: a harness killed by the
    // runner's backstop has produced nothing at all if its results are still in
    // an array, and the check it hung on is exactly what the failure needs to
    // name. The runner parses the stream either way.
    process.stdout.write(`FAIL\t${name}\t${message}\n`);
  } else {
    process.stdout.write(`PASS\t${name}\n`);
  }
}

/** Runs one named check, recording its outcome rather than throwing. */
async function check(name, body) {
  try {
    await within(CHECK_TIMEOUT_MS, `check ${name}`, body());
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
    // Not `response === null ||`: the document request's protocol id *is* the
    // frame's loaderId (ADR-0032 D6a), which is what makes Puppeteer's
    // `isNavigationRequest` true and lets `goto` capture a response at all.
    assert(response !== null, 'page.goto resolved to null: the navigation request was not captured');
    assert(response.ok(), `navigation did not succeed: ${response.status()}`);
    assertEqual(response.request().resourceType(), 'document', 'navigation resourceType');
    assertEqual(page.url(), `${base}/index.html`, 'page.url()');
  });

  await check('response.text of the navigation', async () => {
    const response = await page.goto(`${base}/index.html`);
    // Reads back through `Network.getResponseBody` with the substituted id.
    assert((await response.text()).includes('Fixture'), 'navigation body did not come back');
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
    // `clickCount: 3` is how a driver clears a field: the triple click selects
    // the line, so `type` *replaces* rather than appending to `initial`.
    await page.click('#field', { clickCount: 3 });
    await page.type('#field', 'typed');
    assertEqual(await page.$eval('#field', (el) => el.value), 'typed', 'field value');
  });

  await check('page.hover', async () => {
    await page.goto(`${base}/index.html`);
    await page.hover('#hoverme');
    // Both halves matter: the event fired *and* `:hover` restyled. A driver
    // that only dispatched the event would pass the first and fail the second.
    assertEqual(await page.evaluate(() => globalThis.__hovered), 1, 'mouseover count');
    assertEqual(
      await page.$eval('#hoverme', (el) => getComputedStyle(el).backgroundColor),
      'rgb(0, 128, 0)',
      ':hover background',
    );
  });

  await check('page.select', async () => {
    await page.goto(`${base}/index.html`);
    const selected = await page.select('#choice', 'two');
    assertEqual(selected.join(','), 'two', 'select() return');
    assertEqual(await page.$eval('#choice', (el) => el.value), 'two', 'select value');
  });

  await check('page.$$eval', async () => {
    await page.goto(`${base}/index.html`);
    const texts = await page.$$eval('.para', (els) => els.map((el) => el.textContent));
    assertEqual(texts.join(','), 'first,second', 'paragraph texts');
  });

  await check('elementHandle.boundingBox', async () => {
    await page.goto(`${base}/index.html`);
    const handle = await page.$('#box');
    const box = await handle.boundingBox();
    assert(box, 'no bounding box');
    assertEqual(box.width, 120, 'box width');
    assertEqual(box.height, 60, 'box height');
  });

  await check('page.keyboard.press', async () => {
    await page.goto(`${base}/index.html`);
    await page.focus('#field');
    // `initial` + End + Backspace leaves `initia`; this exercises the named-key
    // path (no text) rather than the printable one `page.type` covers.
    await page.keyboard.press('End');
    await page.keyboard.press('Backspace');
    assertEqual(await page.$eval('#field', (el) => el.value), 'initia', 'field value');
  });

  await check('page.mouse.wheel', async () => {
    await page.goto(`${base}/index.html`);
    await page.mouse.move(50, 50);
    await page.mouse.wheel({ deltaY: 240 });
    assertEqual(await page.evaluate(() => window.scrollY), 240, 'scrollY after wheel');
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

  // === ADR-0032: request interception, file inputs, downloads ===

  await check('setRequestInterception continue', async () => {
    const intercepted = await browser.newPage();
    try {
      await intercepted.setRequestInterception(true);
      const seen = [];
      intercepted.on('request', (request) => {
        seen.push(request.resourceType());
        request.continue();
      });
      const response = await intercepted.goto(`${base}/-/hello`);
      assert(response !== null, 'a continued navigation must still resolve to a response');
      assertEqual(await intercepted.$eval('#p', (el) => el.textContent), 'from the server', 'body');
      assert(seen.includes('document'), `the document itself must pause: ${seen}`);
    } finally {
      await intercepted.close();
    }
  });

  await check('setRequestInterception respond', async () => {
    const intercepted = await browser.newPage();
    try {
      await intercepted.setRequestInterception(true);
      intercepted.on('request', (request) => {
        request.respond({
          status: 200,
          contentType: 'text/html',
          body: '<title>stubbed</title><p id=p>from the driver</p>',
        });
      });
      await intercepted.goto(`${base}/-/hello`);
      assertEqual(await intercepted.title(), 'stubbed', 'title');
      assertEqual(await intercepted.$eval('#p', (el) => el.textContent), 'from the driver', 'body');
    } finally {
      await intercepted.close();
    }
  });

  await check('setRequestInterception abort', async () => {
    const intercepted = await browser.newPage();
    try {
      await intercepted.goto(`${base}/index.html`);
      await intercepted.setRequestInterception(true);
      intercepted.on('request', (request) => {
        // Only the second navigation is blocked, so the page must stay put.
        if (request.url().endsWith('/-/hello')) request.abort('blockedbyclient');
        else request.continue();
      });
      let failed = false;
      try {
        await intercepted.goto(`${base}/-/hello`);
      } catch {
        failed = true;
      }
      assert(failed, 'an aborted navigation must reject');
      assertEqual(intercepted.url(), `${base}/index.html`, 'the page must not have moved');
    } finally {
      await intercepted.close();
    }
  });

  await check('setRequestInterception URL override', async () => {
    const intercepted = await browser.newPage();
    try {
      await intercepted.setRequestInterception(true);
      intercepted.on('request', (request) => {
        if (request.url().endsWith('/-/redirected')) request.continue({ url: `${base}/-/hello` });
        else request.continue();
      });
      await intercepted.goto(`${base}/-/redirected`);
      assertEqual(await intercepted.title(), 'hello', 'the override reached the wire');
    } finally {
      await intercepted.close();
    }
  });

  await check('page.authenticate', async () => {
    const authed = await browser.newPage();
    try {
      await authed.authenticate({ username: 'alice', password: 'secret' });
      const response = await authed.goto(`${base}/-/auth`);
      assertEqual(response.status(), 200, 'the challenge was answered');
      // `alice:secret`, base64.
      const body = JSON.parse(await response.text());
      assertEqual(body.seen, 'Basic YWxpY2U6c2VjcmV0', 'the credentials that reached the server');
    } finally {
      await authed.close();
    }
  });

  await check('emulateNetworkConditions offline', async () => {
    const offline = await browser.newPage();
    try {
      await offline.setOfflineMode(true);
      let failed = false;
      try {
        await offline.goto(`${base}/-/hello`);
      } catch {
        failed = true;
      }
      assert(failed, 'an offline navigation must fail');
      await offline.setOfflineMode(false);
      const response = await offline.goto(`${base}/-/hello`);
      assert(response.ok(), 'and coming back online must restore it');
    } finally {
      await offline.close();
    }
  });

  await check('elementHandle.uploadFile', async () => {
    await page.goto(`${base}/upload.html`);
    const input = await page.$('#file');
    await input.uploadFile(new URL('./fixtures/index.html', import.meta.url).pathname);

    const summary = await page.evaluate(() => window.fileSummary());
    assertEqual(summary.length, 1, 'files.length');
    assertEqual(summary.names[0], 'index.html', 'the selected name');
    assert(summary.sizes[0] > 0, 'the file has bytes');
    assert(summary.isFile && summary.isBlob, 'a File must also be a Blob');

    // Trusted `input` then `change`, in that order — what every upload widget
    // listens for.
    const events = await page.evaluate(() => JSON.stringify(window.events));
    assertEqual(events, '[["input",true],["change",true]]', 'the selection events');
  });

  await check('a file input forces a multipart post', async () => {
    await page.goto(`${base}/upload.html`);
    const input = await page.$('#file');
    await input.uploadFile(new URL('./fixtures/index.html', import.meta.url).pathname);
    await Promise.all([
      page.waitForNavigation(),
      page.click('#submit'),
    ]);
    const echoed = JSON.parse(await page.evaluate(() => document.body.textContent));
    assertEqual(echoed.method, 'POST', 'method');
    // The form declares no enctype: a non-empty file input forces multipart.
    assert(echoed.contentType.startsWith('multipart/form-data;'), `enctype: ${echoed.contentType}`);
    assertEqual(echoed.filenames.join(','), 'index.html', 'the file part names its filename');
    assert(echoed.body.includes('name="field"'), 'the ordinary field survived');
    assert(echoed.body.includes('OxidePage automation fixture'), 'the bytes were sent');
  });

  await check('page.waitForFileChooser', async () => {
    await page.goto(`${base}/upload.html`);
    const [chooser] = await Promise.all([
      page.waitForFileChooser(),
      page.click('#multi'),
    ]);
    assert(chooser.isMultiple(), 'the chooser must report the multiple attribute');
    await chooser.accept([new URL('./fixtures/other.html', import.meta.url).pathname]);
    const names = await page.evaluate(() =>
      [...document.getElementById('multi').files].map((f) => f.name).join(','));
    assertEqual(names, 'other.html', 'the accepted selection');
  });

  await check('a download does not commit a document', async () => {
    // Every step bounded and named, unlike the rest of the suite: this check
    // hung in CI (twice in three runs) and the check-level budget could only say
    // *that* it hung, not where. A step that blows its bound now names itself.
    const downloads = await within(10_000, 'browser.newPage', browser.newPage());
    try {
      await within(10_000, 'goto index.html', downloads.goto(`${base}/index.html`));
      const client = await within(10_000, 'createCDPSession', downloads.createCDPSession());
      await within(
        10_000,
        'Browser.setDownloadBehavior',
        client.send('Browser.setDownloadBehavior', {
          behavior: 'allow',
          downloadPath: downloadDir,
        }),
      );
      const began = new Promise((resolve) => {
        client.on('Page.downloadWillBegin', resolve);
      });
      await within(10_000, 'Page.enable', client.send('Page.enable'));
      // The navigation answers rather than committing; the document stays. It
      // is *allowed* to reject — Chrome aborts a download navigation and
      // Puppeteer surfaces that — but it must settle either way.
      await within(
        10_000,
        'the download navigation to settle',
        downloads.goto(`${base}/-/attachment`).catch(() => {}),
      );
      const event = await within(10_000, 'Page.downloadWillBegin', began);
      assertEqual(event.suggestedFilename, 'report.csv', 'suggestedFilename');
      assertEqual(downloads.url(), `${base}/index.html`, 'the document must not have moved');
      assertEqual(
        await fs.readFile(path.join(downloadDir, 'report.csv'), 'utf8'),
        'a,b\n1,2\n',
        'the download landed on disk',
      );
    } finally {
      // Bounded like the rest: a `close` that never answers would otherwise
      // report as a failure of whatever the check last awaited.
      await within(10_000, 'page.close', downloads.close());
    }
  });

  // === Isolated worlds (ADR-0033) ==========================================
  //
  // Driven over a raw CDP session rather than through Puppeteer's own helpers,
  // because Puppeteer hides its utility world entirely — the point here is to
  // assert the isolation the driver relies on, not to re-test the driver.

  await check('an isolated world is really isolated', async () => {
    const worlds = await browser.newPage();
    try {
      await worlds.goto(`${base}/index.html`);
      const client = await worlds.createCDPSession();
      const { executionContextId } = await client.send('Page.createIsolatedWorld', {
        worldName: 'probe',
      });

      // Page globals are not visible in the world…
      const fromPage = await client.send('Runtime.evaluate', {
        expression: 'typeof globalThis.__ready',
        contextId: executionContextId,
        returnByValue: true,
      });
      assertEqual(fromPage.result.value, 'undefined', 'a page global leaked into the world');

      // …and the world's globals are not visible to the page.
      await client.send('Runtime.evaluate', {
        expression: 'globalThis.__onlyHere = 1',
        contextId: executionContextId,
      });
      const leaked = await worlds.evaluate(() => typeof globalThis.__onlyHere);
      assertEqual(leaked, 'undefined', "the world's global leaked into the page");

      // The DOM underneath is the same one.
      const shared = await client.send('Runtime.evaluate', {
        expression: "document.getElementById('heading').textContent",
        contextId: executionContextId,
        returnByValue: true,
      });
      assertEqual(shared.result.value, 'Fixture', 'the world sees a different DOM');
    } finally {
      await worlds.close();
    }
  });

  await check('an init script with a worldName runs only in that world', async () => {
    const worlds = await browser.newPage();
    try {
      const client = await worlds.createCDPSession();
      await client.send('Page.addScriptToEvaluateOnNewDocument', {
        source: 'globalThis.__injected = "yes"',
        worldName: 'probe',
      });
      await worlds.goto(`${base}/index.html`);

      // The world is rebuilt at the commit, so its context id is a new one.
      const { executionContextId } = await client.send('Page.createIsolatedWorld', {
        worldName: 'probe',
      });
      const inWorld = await client.send('Runtime.evaluate', {
        expression: 'globalThis.__injected',
        contextId: executionContextId,
        returnByValue: true,
      });
      assertEqual(inWorld.result.value, 'yes', 'the init script did not run in its world');
      assertEqual(
        await worlds.evaluate(() => typeof globalThis.__injected),
        'undefined',
        'a worldName init script leaked into the main world',
      );
    } finally {
      await worlds.close();
    }
  });

  await check('a binding in a named world is not on the page global', async () => {
    const worlds = await browser.newPage();
    try {
      await worlds.goto(`${base}/index.html`);
      const client = await worlds.createCDPSession();
      await client.send('Runtime.addBinding', {
        name: '__reportFromWorld',
        executionContextName: 'probe',
      });
      assertEqual(
        await worlds.evaluate(() => typeof globalThis.__reportFromWorld),
        'undefined',
        'a world-scoped binding was installed on the page global',
      );

      const { executionContextId } = await client.send('Page.createIsolatedWorld', {
        worldName: 'probe',
      });
      const called = new Promise((resolve) => client.on('Runtime.bindingCalled', resolve));
      await client.send('Runtime.evaluate', {
        expression: '__reportFromWorld("hi")',
        contextId: executionContextId,
      });
      const event = await within(10_000, 'Runtime.bindingCalled', called);
      assertEqual(event.payload, 'hi', 'binding payload');
      assertEqual(
        event.executionContextId,
        executionContextId,
        'the call must be attributed to the world it came from',
      );
    } finally {
      await worlds.close();
    }
  });

  await check('Blob and FileReader', async () => {
    await page.goto(`${base}/index.html`);
    const result = await page.evaluate(async () => {
      const blob = new Blob(['hello ', 'world'], { type: 'Text/Plain' });
      const sliced = blob.slice(6);
      const read = await new Promise((resolve) => {
        const reader = new FileReader();
        reader.onloadend = () => resolve(reader.result);
        reader.readAsText(sliced);
      });
      return JSON.stringify([blob.size, blob.type, sliced.size, read, await blob.text()]);
    });
    assertEqual(result, '[11,"text/plain",5,"world","hello world"]', 'Blob + FileReader');
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
}

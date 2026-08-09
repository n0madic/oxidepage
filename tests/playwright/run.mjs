// The Playwright conformance harness. Driven by `cargo xtask playwright`, which
// starts the endpoint and a loopback fixture server and passes both here.
//
// Output contract: one `PASS\t<name>` or `FAIL\t<name>\t<message>` line per
// check, on stdout, nothing else — identical to `tests/automation/run.mjs`, so
// the two share `xtask/src/nodeharness.rs` and the same two-sided expectation
// rule as WPT: a regression *and* an unexpected pass both fail CI.
//
// `playwright-core`, not `playwright`: the latter downloads browser binaries at
// install time, and the whole point here is to drive OxidePage over CDP.
//
// Playwright runs **all** of its injected script in a utility world created
// with `Page.createIsolatedWorld`, and `addInitScript` and `exposeBinding` ride
// the same mechanism — so nothing below works at all without real isolated
// worlds (stage 9, ADR-0033). Stage 10 (ADR-0034) took the suite to green:
// `expectations.tsv` is empty, so any line appearing in it is a regression
// somebody wrote down instead of fixing.

import { chromium } from 'playwright-core';

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

/**
 * How long any one check may take before it is failed instead of left to hang
 * the whole harness. See `tests/automation/run.mjs` — same contract, same
 * reason, and it has to be the same in both or the runners diverge.
 */
const CHECK_TIMEOUT_MS = 30000;

function report(name, error) {
  if (error) {
    const message = String(error && error.message ? error.message : error)
      .replace(/[\r\n\t]+/g, ' ')
      .slice(0, 300);
    // Written as it happens rather than collected for the end, so a harness
    // killed by the runner's backstop still names where it stopped.
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

/// Fails the current check cleanly when an earlier one left `page` unset.
///
/// Without this a later check dereferences `undefined` and the *harness*
/// crashes, which produces no results at all — and a runner with no results
/// cannot be compared against `expectations.tsv`. Every check must fail as a
/// check.
function requirePage() {
  if (!page) throw new Error('no page: an earlier check failed to create one');
  return page;
}

function assertEqual(actual, expected, what) {
  if (actual !== expected) {
    throw new Error(`${what}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
  }
}

/** A promise that rejects if `inner` has not settled within `ms`.
 *
 * The connect/newPage/goto budgets are generous on purpose: they gate every
 * other check, so a timeout there reports every later failure as its own. They
 * are no longer covering for a race — `Target.attachedToTarget` now precedes
 * the `createTarget` reply it belongs to (ADR-0034 D7), which is what the
 * "known flake" actually was — and CI runs this as a job of its own rather
 * than after the WPT sweep. */
function within(ms, what, inner) {
  let timer;
  const guard = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(`timed out after ${ms}ms waiting for ${what}`)), ms);
  });
  return Promise.race([inner, guard]).finally(() => clearTimeout(timer));
}

// `connectOverCDP` is the only entry point that does not launch a browser
// binary, and it is how Playwright attaches to an existing endpoint.
let browser;
try {
  browser = await within(60000, 'chromium.connectOverCDP', chromium.connectOverCDP(endpoint));
  report('chromium.connectOverCDP');
} catch (error) {
  report('chromium.connectOverCDP', error);
  process.exit(0);
}

let page;
let context;
try {
  await check('browser.contexts', async () => {
    const contexts = browser.contexts();
    assert(contexts.length >= 1, `expected at least one context, got ${contexts.length}`);
    context = contexts[0];
  });

  await check('context.newPage', async () => {
    page = await within(60000, 'newPage', context.newPage());
    assert(page, 'newPage returned nothing');
  });

  await check('page.goto', async () => {
    const response = await within(60000, 'goto', requirePage().goto(`${base}/index.html`));
    assert(response !== null, 'goto resolved to null');
    assert(response.ok(), `navigation did not succeed: ${response.status()}`);
    assertEqual(requirePage().url(), `${base}/index.html`, 'page.url()');
  });

  await check('page.title', async () => {
    assertEqual(await requirePage().title(), 'OxidePage Playwright fixture', 'page.title()');
  });

  // `evaluate` runs in the *main* world, unlike everything Playwright injects
  // for itself — so this and the isolation check below are two different paths.
  await check('page.evaluate', async () => {
    assertEqual(await requirePage().evaluate(() => document.title), 'OxidePage Playwright fixture', 'evaluate');
    assertEqual(await requirePage().evaluate(() => globalThis.__ready), true, 'page global');
    assertEqual(await requirePage().evaluate(([a, b]) => a + b, [2, 3]), 5, 'evaluate with an argument');
  });

  await check('page.textContent', async () => {
    assertEqual(await requirePage().textContent('#heading'), 'Fixture', 'textContent');
  });

  await check('locator.click with an asserted side effect', async () => {
    await requirePage().locator('#tap').click();
    assertEqual(await requirePage().evaluate(() => globalThis.__taps), 1, 'click handler ran');
    assertEqual(await requirePage().textContent('#heading'), 'tapped', 'the click changed the DOM');
  });

  await check('page.fill', async () => {
    await requirePage().fill('#field', 'typed');
    assertEqual(await requirePage().inputValue('#field'), 'typed', 'inputValue');
  });

  await check('locator.count', async () => {
    assertEqual(await requirePage().locator('.para').count(), 2, 'locator count');
  });

  await check('page.waitForSelector', async () => {
    await requirePage().evaluate(() => {
      setTimeout(() => {
        const el = document.createElement('div');
        el.id = 'late';
        // Text, not an empty div. `waitForSelector` defaults to
        // `state: 'visible'`, and Playwright's `isElementVisible` requires a
        // non-empty bounding box — an unstyled empty `<div>` is 0px high and
        // stays "hidden" forever. That is not an engine limitation: the check
        // would time out against real Chrome too.
        el.textContent = 'late';
        document.body.appendChild(el);
      }, 20);
    });
    const handle = await within(10000, 'waitForSelector', requirePage().waitForSelector('#late'));
    assert(handle, 'waitForSelector resolved to nothing');
    assertEqual(await handle.textContent(), 'late', 'the element it waited for');
  });

  // The stage's own milestone: Playwright's injected helpers live in a utility
  // world, so this asserts the isolation that makes them safe from page script.
  await check('addInitScript survives a navigation', async () => {
    await requirePage().addInitScript(() => {
      globalThis.__injected = 'yes';
    });
    await requirePage().goto(`${base}/other.html`);
    assertEqual(await requirePage().evaluate(() => globalThis.__injected), 'yes', 'init script global');
  });

  await check('page.exposeBinding', async () => {
    await requirePage().exposeBinding('__addTwo', (_source, n) => n + 2);
    await requirePage().goto(`${base}/index.html`);
    assertEqual(await requirePage().evaluate(() => globalThis.__addTwo(40)), 42, 'exposed binding');
  });

  await check('page.goBack', async () => {
    await requirePage().goto(`${base}/other.html`);
    await requirePage().goBack();
    assertEqual(requirePage().url(), `${base}/index.html`, 'url after goBack');
  });

  await check('page.setContent', async () => {
    await requirePage().setContent('<!doctype html><title>set</title><p id=s>content</p>');
    assertEqual(await requirePage().textContent('#s'), 'content', 'setContent');
  });

  // `page.route` is the milestone's "routes". Playwright routes through
  // `Fetch.enable { handleAuthRequests: true, patterns: [{ urlPattern: "*",
  // requestStage: "Request" }] }` and correlates `requestPaused.networkId`
  // with `Network.requestWillBeSent.requestId` — which works here because the
  // engine hands out the same id for both (ADR-0032).
  await check('page.route fulfills', async () => {
    const target = requirePage();
    await target.route('**/-/hello', (route) =>
      route.fulfill({ status: 200, contentType: 'text/html', body: '<p id=r>fulfilled</p>' }));
    await target.goto(`${base}/-/hello`);
    assertEqual(await target.textContent('#r'), 'fulfilled', 'the fulfilled body');
    await target.unroute('**/-/hello');
  });

  await check('page.route continues', async () => {
    const target = requirePage();
    let seen = 0;
    await target.route('**/-/hello', (route) => {
      seen += 1;
      return route.continue();
    });
    await target.goto(`${base}/-/hello`);
    assertEqual(seen, 1, 'the handler ran once');
    // Continued unmodified, so the *server's* document is what loaded.
    assertEqual(await target.textContent('#p'), 'from the server', 'the served body');
    await target.unroute('**/-/hello');
  });

  await check('page.route aborts', async () => {
    const target = requirePage();
    await target.goto(`${base}/index.html`);
    await target.route('**/-/hello', (route) => route.abort());
    let failed = false;
    try {
      await target.goto(`${base}/-/hello`);
    } catch {
      failed = true;
    }
    assert(failed, 'an aborted navigation must fail rather than load');
    await target.unroute('**/-/hello');
  });

  await check('page.screenshot', async () => {
    const shot = await requirePage().screenshot();
    assert(shot.length > 0, 'screenshot produced no bytes');
    assertEqual(shot[0], 0x89, 'not a PNG');
  });

  await check('console message', async () => {
    // `requirePage()` first: a throw inside the `new Promise` executor below
    // would leave a rejected promise nobody awaits, and Node kills the process
    // on the unhandled rejection — taking every result with it.
    const target = requirePage();
    const seen = new Promise((resolve) => target.once('console', (m) => resolve(m.text())));
    await target.evaluate(() => console.log('from the page'));
    assertEqual(await within(10000, 'console event', seen), 'from the page', 'console text');
  });
  await check('page.frames sees a nested browsing context', async () => {
    const target = requirePage();
    await target.goto(`${base}/frames.html`);
    const frames = target.frames();
    assertEqual(String(frames.length), '2', 'the page frame plus the iframe');
    const child = frames.find((f) => f !== target.mainFrame());
    if (!child) throw new Error('no child frame');
    assertEqual(String(child.parentFrame() === target.mainFrame()), 'true', 'parent');
  });

  await check('frame.evaluate runs in the frame own realm', async () => {
    const target = requirePage();
    await target.goto(`${base}/frames.html`);
    const child = target.frames().find((f) => f !== target.mainFrame());
    // `other.html` is what the `<iframe src>` loaded — the frame's own
    // document, not the embedder's. (Its title and heading are deliberately
    // each other's near-miss, so a check that read the wrong document would
    // still fail rather than coincide.)
    assertEqual(await child.evaluate(() => document.title), 'Other', 'own document');
    assertEqual(
      await child.evaluate(() => document.querySelector('h1').textContent),
      'Other page',
      'own tree',
    );
  });

  // Expected to fail, **cause not yet identified**. `page.frames()` and
  // `frame.evaluate()` above pass, so the frame, its realm and its execution
  // context all reach the driver. Two candidates were implemented and ruled
  // out: `Page.createIsolatedWorld` now honours `frameId` (so a locator does
  // get a utility world of the right frame) and `DOM.getFrameOwner` answers.
  // Whatever `frameLocator` is still waiting on is something else — recorded
  // as an honest unknown rather than a wrong explanation.
  await check('frameLocator reaches into the frame', async () => {
    const target = requirePage();
    await target.goto(`${base}/frames.html`);
    const text = await target.frameLocator('#inner').locator('h1').textContent();
    assertEqual(text, 'Other page', 'frameLocator resolved through the <iframe>');
  });

} finally {
  // `close`, not a teardown of the endpoint: the runner owns the server and
  // stops it when this process exits.
  try {
    await browser.close();
  } catch {
    // The socket may already be gone; that is not a check result.
  }
}

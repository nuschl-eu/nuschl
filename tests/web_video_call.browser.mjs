// SPDX-License-Identifier: EUPL-1.2
// Two Chromium clients in the web VM. The call page's media transport runs
// against the private SFU, and the Mumble tunnel carries real chat and cards.
import assert from "node:assert/strict";
import { pathToFileURL } from "node:url";

const [app, host, port] = process.argv.slice(2);
assert.ok(app && host && port, "owned app URL, Murmur host, and port required");
const { chromium } = await import(pathToFileURL(process.env.NUSCHL_PLAYWRIGHT).href);
const appOrigin = new URL(app).origin;
const browser = await chromium.launch({
  headless: true, executablePath: process.env.NUSCHL_CHROMIUM,
  args: [
    `--unsafely-treat-insecure-origin-as-secure=${appOrigin}`,
    "--use-fake-device-for-media-stream", "--use-fake-ui-for-media-stream",
  ],
});
const errors = [];
const pages = [];

async function joinApp(name) {
  const context = await browser.newContext({ ignoreHTTPSErrors: true });
  context.setDefaultTimeout(60000);
  await context.addInitScript(() => {
    const media = navigator.mediaDevices;
    const camera = media.getUserMedia.bind(media);
    const video = { width: 320, height: 180, frameRate: 15 };
    window.videoCaptureCalls = [];
    media.getUserMedia = (options) => {
      videoCaptureCalls.push(options.audio ? "microphone" : "camera");
      return camera({ video, audio: options.audio });
    };
    // Simulate the result of the screen picker with a second synthetic track.
    media.getDisplayMedia = (options) => {
      videoCaptureCalls.push(options.audio ? "screen audio" : "screen");
      return camera({ video, audio: options.audio });
    };
  });
  const page = await context.newPage();
  pages.push(page);
  page.on("pageerror", error => errors.push(error.message));
  page.on("console", message => {
    if (message.type() === "error") console.error("Chromium:", message.text());
  });
  await page.goto(`${app}#s=${host}:${port}&u=${name}`);
  await page.getByRole("button", { name: "Trust this certificate", exact: true }).click();
  await page.waitForFunction(() => document.querySelector(".server-connection .status")?.textContent === "Connected");
  return page;
}

try {
  const hostPage = await joinApp("video-host");
  const guest = await joinApp("video-guest");
  const nextLease = hostPage.waitForResponse(response => new URL(response.url()).pathname === "/sfu/lease");
  nextLease.catch(() => {});
  await hostPage.getByRole("button", { name: "Start call", exact: true }).click();
  const joined = page => page.waitForFunction(() =>
    /AV1|VP9/.test(document.querySelector(".video-session #status")?.textContent ?? ""));
  await joined(hostPage);
  await guest.getByRole("button", { name: "Join call", exact: true }).first().click();
  await joined(guest);
  assert.deepEqual(await hostPage.evaluate(() => videoCaptureCalls), []);
  assert.deepEqual(await guest.evaluate(() => videoCaptureCalls), []);

  await hostPage.getByText("Advanced", { exact: true }).click();
  await hostPage.getByLabel("Camera limit").selectOption("0");
  await hostPage.getByLabel("Screen limit").selectOption("0");
  await hostPage.getByRole("button", { name: "Start camera", exact: true }).click();
  await hostPage.getByRole("button", { name: "Share screen", exact: true }).click();
  await guest.waitForFunction(() => {
    const videos = [...document.querySelectorAll("#stage .tile video")];
    return videos.length === 2 && videos.every(video => video.readyState >= 2 && video.videoWidth > 0);
  });
  assert.deepEqual(await hostPage.evaluate(() => videoCaptureCalls), ["camera", "screen"]);
  await hostPage.getByRole("button", { name: "Minimize", exact: true }).click();
  await hostPage.getByRole("button", { name: "Lobby", exact: true }).click();
  await hostPage.getByRole("button", { name: "Show video", exact: true }).click();
  await hostPage.getByRole("button", { name: "Stop sharing", exact: true }).waitFor();
  const lease = await nextLease;
  assert.equal(lease.request().method(), "POST");
  assert.equal(lease.status(), 204);
  await hostPage.getByRole("button", { name: "End call", exact: true }).click();
  await guest.getByRole("alert").getByText("Call ended", { exact: true }).waitFor();
  await guest.locator("article").getByText("Call ended", { exact: true }).waitFor();
  assert.deepEqual(errors, []);
  console.log("web app: camera and screen decoded, chat card and call lifecycle passed");
} catch (error) {
  for (const page of pages) {
    const alerts = await page.locator(".error, .video-session #ending, .video-session #status").allTextContents();
    console.error("Call status:", alerts.join(" "));
  }
  throw error;
} finally {
  await browser.close();
}

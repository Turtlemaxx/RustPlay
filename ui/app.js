"use strict";

const tauri = window.__TAURI__;
const appWindow = tauri?.window.getCurrentWindow();
const reducedMotion = matchMedia("(prefers-reduced-motion: reduce)");

const $ = (selector) => document.querySelector(selector);
const body = document.body;
const tile = $(".tile");
const screenEl = $(".screen");
const cover = $(".cover");
const canvas = $(".screen canvas");
const ctx = canvas.getContext("2d", { alpha: false, desynchronized: true });
const layers = { bg: $(".tile .bg"), airplay: $(".tile .airplay"), crab: $(".tile .crab"), glint: $(".tile .glint i") };
const rim = $(".tile .rim");

// Where the squircle sits inside the icon art (see icons/draw_icon.py).
const ICON_INSET = 150 / 2048;
const ICON_RADIUS = 430 / (2048 - 300);

const clamp = (v, lo, hi) => Math.min(hi, Math.max(lo, v));
const smoothstep = (a, b, v) => {
  const t = clamp((v - a) / (b - a), 0, 1);
  return t * t * (3 - 2 * t);
};

/* ── Springs ──────────────────────────────────────────────
   A damped spring per animated value, parameterized like SwiftUI
   (response in seconds, damping fraction). Retargeting keeps the current
   velocity, so every animation can be interrupted mid-flight. */

class Spring {
  constructor(value, precision = 0.01) {
    this.value = this.target = value;
    this.velocity = 0;
    this.precision = precision;
    this.configure(0.5, 1);
  }

  configure(response, damping) {
    const omega = (2 * Math.PI) / response;
    this.stiffness = omega * omega;
    this.damping = 2 * damping * omega;
    return this;
  }

  step(dt) {
    const n = Math.ceil(dt * 480);
    const h = dt / n;
    for (let i = 0; i < n; i++) {
      const force = -this.stiffness * (this.value - this.target) - this.damping * this.velocity;
      this.velocity += force * h;
      this.value += this.velocity * h;
    }
  }

  get resting() {
    return Math.abs(this.target - this.value) < this.precision && Math.abs(this.velocity) < this.precision * 10;
  }

  snap(value = this.target) {
    this.value = this.target = value;
    this.velocity = 0;
  }
}

const running = new Set();
let frameRequest = 0;
let lastFrame = 0;

function run(item) {
  if (reducedMotion.matches) {
    item.springs.forEach((s) => s.snap());
    item.render();
    finish(item);
    return;
  }
  running.add(item);
  if (!frameRequest) {
    lastFrame = performance.now();
    frameRequest = requestAnimationFrame(tick);
  }
}

function tick(now) {
  const dt = Math.min((now - lastFrame) / 1000, 1 / 20);
  lastFrame = now;
  for (const item of running) {
    let moving = false;
    for (const spring of item.springs) {
      spring.step(dt);
      if (spring.resting) spring.snap();
      else moving = true;
    }
    item.render();
    if (!moving) {
      running.delete(item);
      finish(item);
    }
  }
  frameRequest = running.size ? requestAnimationFrame(tick) : 0;
}

function finish(item) {
  const done = item.done;
  item.done = null;
  done?.();
}

// The same physics sampled into CSS linear() easings, for CSS and one-shot animations.
function cssSpring(response, damping) {
  const s = new Spring(0, 0.0005).configure(response, damping);
  s.target = 1;
  const points = [0];
  const dt = 1 / 60;
  while (!s.resting && points.length < 240) {
    s.step(dt);
    points.push(+s.value.toFixed(4));
  }
  points.push(1);
  return { easing: `linear(${points.join(", ")})`, duration: Math.round((points.length - 1) * dt * 1000) };
}
const curves = { smooth: cssSpring(0.5, 1), snappy: cssSpring(0.32, 0.9), bouncy: cssSpring(0.45, 0.68) };
for (const [name, { easing, duration }] of Object.entries(curves)) {
  document.documentElement.style.setProperty(`--${name}`, easing);
  document.documentElement.style.setProperty(`--${name}-dur`, `${duration}ms`);
}

function play(element, keyframes, curve, options = {}) {
  if (reducedMotion.matches) return null;
  return element.animate(keyframes, { duration: curve.duration, easing: curve.easing, ...options });
}

/* ── Icon ─────────────────────────────────────────────────
   tvOS-style parallax: the icon tilts toward the pointer and its front layers
   rise and grow over the ones behind. A Liquid Glass-style specular light sits
   on the rim, facing the pointer. */

const DEFAULT_LIGHT = -35; // degrees; Apple's light comes from the top left

const tilt = {
  px: new Spring(0, 0.001).configure(0.5, 0.78),
  py: new Spring(0, 0.001).configure(0.5, 0.78),
  lift: new Spring(0, 0.001).configure(0.36, 0.68),
  press: new Spring(0, 0.001).configure(0.22, 0.75),
  render() {
    const px = this.px.value;
    const py = this.py.value;
    const lift = this.lift.value;
    const angle = 3 + lift * 7;
    const scale = 1 + lift * 0.07 - this.press.value * 0.07;
    tile.style.transform =
      `perspective(800px) rotateX(${(-py * angle).toFixed(3)}deg) rotateY(${(px * angle).toFixed(3)}deg) scale(${scale.toFixed(4)})`;
    layers.airplay.style.transform = `translate(${(px * 4).toFixed(2)}px, ${(py * 4).toFixed(2)}px) scale(${(1 + lift * 0.025).toFixed(4)})`;
    layers.crab.style.transform = `translate(${(px * 10).toFixed(2)}px, ${(py * 10).toFixed(2)}px) scale(${(1 + lift * 0.06).toFixed(4)})`;

    // Light faces the pointer; near the center it eases back to the default.
    const reach = clamp(Math.hypot(px, py) * 3, 0, 1);
    const pointerAngle = (Math.atan2(px, -py) * 180) / Math.PI;
    let delta = ((pointerAngle - DEFAULT_LIGHT + 540) % 360) - 180;
    tile.style.setProperty("--light", `${(DEFAULT_LIGHT + delta * reach).toFixed(1)}deg`);
    tile.style.setProperty("--rim", (0.6 + lift * 0.4).toFixed(3));
    tile.style.setProperty("--lift", lift.toFixed(3));
    tile.style.setProperty("--sheen", (0.25 + lift * 0.75).toFixed(3));
    tile.style.setProperty("--sx", `${(50 + px * 50).toFixed(1)}%`);
    tile.style.setProperty("--sy", `${(50 + py * 50).toFixed(1)}%`);
  },
};
tilt.springs = [tilt.px, tilt.py, tilt.lift, tilt.press];

let hovering = false;
addEventListener("pointermove", (event) => {
  if (phase === "live" || section !== "airplay" || body.classList.contains("settings-open")) return;
  const r = tile.getBoundingClientRect();
  const cx = r.left + r.width / 2;
  const cy = r.top + r.height / 2;
  const reach = Math.max(innerWidth, innerHeight) * 0.42;
  tilt.px.target = clamp((event.clientX - cx) / reach, -1, 1);
  tilt.py.target = clamp((event.clientY - cy) / reach, -1, 1);
  const over = Math.abs(event.clientX - cx) < r.width * 0.45 && Math.abs(event.clientY - cy) < r.height * 0.45;
  if (over && !hovering) glint(0.75);
  hovering = over;
  tilt.lift.target = over ? 1 : 0;
  run(tilt);
});
document.documentElement.addEventListener("pointerleave", () => restTile());
tile.addEventListener("pointerdown", () => {
  tilt.press.target = 1;
  run(tilt);
});
addEventListener("pointerup", () => {
  tilt.press.target = 0;
  run(tilt);
});

function restTile() {
  hovering = false;
  tilt.px.target = tilt.py.target = tilt.lift.target = tilt.press.target = 0;
  run(tilt);
}

/** A band of light sweeping across the glass, as when an icon catches the light. */
function glint(strength = 1) {
  play(
    layers.glint,
    [
      { transform: "translateX(-75%)", opacity: strength },
      { transform: "translateX(75%)", opacity: strength },
    ],
    { duration: 1100, easing: "cubic-bezier(0.45, 0, 0.2, 1)" },
  );
  play(rim, [{ filter: "brightness(1)" }, { filter: `brightness(${1 + strength})` }, { filter: "brightness(1)" }], {
    duration: 1100,
    easing: "ease-in-out",
  });
}

/** The icon assembling itself on launch: background, then glyph, then Ferris. */
function introduce() {
  play(layers.bg, [{ scale: 0.55, opacity: 0 }, { scale: 1, opacity: 1 }], curves.bouncy, { fill: "backwards" });
  play(layers.airplay, [{ scale: 0.6, opacity: 0 }, { scale: 1, opacity: 1 }], curves.bouncy, { delay: 90, fill: "backwards" });
  play(layers.crab, [{ scale: 0.2, opacity: 0, translate: "0 18px" }, { scale: 1, opacity: 1, translate: "0 0" }], cssSpring(0.5, 0.55), {
    delay: 170,
    fill: "backwards",
  });
  setTimeout(() => glint(), 650);
  setTimeout(() => body.classList.remove("intro"), 1400);
}

/** SF Symbols-style wiggle, for Ferris when a device starts connecting. */
function wiggle() {
  play(
    layers.crab,
    [{ rotate: "0deg" }, { rotate: "-10deg" }, { rotate: "9deg" }, { rotate: "-6deg" }, { rotate: "3deg" }, { rotate: "0deg" }],
    { duration: 700, easing: "ease-in-out" },
  );
}

/** The icon settling after the mirrored screen zooms back into it. */
function settle() {
  play(tile, [{ scale: 0.93 }, { scale: 1 }], cssSpring(0.42, 0.55));
  play(layers.crab, [{ translate: "0 0" }, { translate: "0 -10px" }, { translate: "0 0" }], { duration: 420, easing: "cubic-bezier(0.3, 0, 0.3, 1)" }, { delay: 60 });
  setTimeout(() => glint(0.8), 120);
}

// Every so often, while nobody is interacting, the glass catches the light.
setInterval(() => {
  if (phase === "idle" && section === "airplay" && !hovering && !document.hidden && !body.classList.contains("renaming")) glint(0.6);
}, 9000);

/* ── Screen geometry: zooms out of the icon, like an app launch ── */

const geo = {
  x: new Spring(0, 0.05),
  y: new Spring(0, 0.05),
  w: new Spring(0, 0.05),
  h: new Spring(0, 0.05),
  r: new Spring(0, 0.05),
  iconSize: 1,
  fullSize: 1,
  render() {
    screenEl.style.transform = `translate(${this.x.value.toFixed(2)}px, ${this.y.value.toFixed(2)}px)`;
    screenEl.style.width = `${Math.max(1, this.w.value).toFixed(2)}px`;
    screenEl.style.height = `${Math.max(1, this.h.value).toFixed(2)}px`;
    screenEl.style.borderRadius = `${Math.max(0, this.r.value).toFixed(2)}px`;
    // The icon art fades out over the first part of the zoom (and back in at the end of a close).
    const progress = (this.w.value - this.iconSize) / Math.max(1, this.fullSize - this.iconSize);
    cover.style.opacity = (1 - smoothstep(0.04, 0.4, progress)).toFixed(3);
  },
};
geo.springs = [geo.x, geo.y, geo.w, geo.h, geo.r];

function setGeo(rect, response, damping, done) {
  for (const key of ["x", "y", "w", "h", "r"]) {
    geo[key].configure(response, damping);
    geo[key].target = rect[key];
  }
  geo.done = done ?? null;
  run(geo);
}

function jumpGeo(rect) {
  for (const key of ["x", "y", "w", "h", "r"]) geo[key].snap(rect[key]);
  geo.render();
}

function iconRect() {
  const r = tile.getBoundingClientRect();
  const inset = r.width * ICON_INSET;
  const size = r.width - inset * 2;
  return { x: r.left + inset, y: r.top + inset, w: size, h: size, r: size * ICON_RADIUS };
}

function screenRect() {
  const fullscreen = body.classList.contains("fullscreen");
  const pad = fullscreen ? 0 : 24;
  const top = fullscreen ? 0 : 44;
  const availW = innerWidth - pad * 2;
  const availH = innerHeight - top - pad * 2;
  const scale = Math.min(availW / videoSize.width, availH / videoSize.height);
  const w = videoSize.width * scale;
  const h = videoSize.height * scale;
  return { x: (innerWidth - w) / 2, y: top + pad + (availH - h) / 2, w, h, r: fullscreen ? 0 : Math.min(w, h) * 0.085 };
}

let screenOpen = false;

function openScreen() {
  const target = screenRect();
  if (!screenEl.classList.contains("shown")) {
    restTile();
    tilt.springs.forEach((s) => s.snap(0));
    tilt.render();
    jumpGeo(iconRect());
    screenEl.classList.add("shown");
    tile.classList.add("morphing");
  }
  screenOpen = true;
  geo.iconSize = tile.getBoundingClientRect().width * (1 - 2 * ICON_INSET);
  geo.fullSize = target.w;
  setGeo(target, 0.55, 0.8);
}

function closeScreen() {
  if (!screenEl.classList.contains("shown")) return;
  screenOpen = false;
  const target = iconRect();
  geo.iconSize = target.w;
  geo.fullSize = Math.max(geo.w.value, target.w + 1);
  setGeo(target, 0.48, 0.86, () => {
    if (screenOpen) return;
    screenEl.classList.remove("shown", "reflow");
    tile.classList.remove("morphing");
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    settle();
  });
}

let lastResize = 0;
addEventListener("resize", () => {
  const now = performance.now();
  if (screenOpen) {
    // Follow a dragged edge directly; spring into a one-off change like full screen.
    if (now - lastResize < 150) jumpGeo(screenRect());
    else setGeo(screenRect(), 0.42, 0.92);
  }
  lastResize = now;
  syncFullscreen();
});

/* ── Text that blurs between values ──────────────────── */

function swapText(container, text) {
  const current = container.querySelector("span:not(.out)");
  if (current && current.textContent === text) return;
  const next = document.createElement("span");
  next.textContent = text;
  next.className = "in";
  container.append(next);
  if (current) {
    current.classList.add("out");
    setTimeout(() => current.remove(), 400);
  }
  requestAnimationFrame(() => requestAnimationFrame(() => next.classList.remove("in")));
}

/* ── Phases ──────────────────────────────────────────── */

let phase = "idle";
let deviceName = null;
let receiverName = "RustPlay";

function setPhase(next) {
  if (phase === next) return;
  phase = next;
  body.dataset.phase = next;
  showHud(next === "live");
}

function applyStatus(status) {
  deviceName = status.device;
  if (status.receiver !== receiverName || !$(".name-text span")) {
    receiverName = status.receiver;
    swapText($(".name-text"), receiverName);
    $(".name-button").setAttribute("aria-label", `Rename ${receiverName}`);
    setAliasNames(receiverName);
  }
  if (typeof status.volume === "number" && !slider.dragging) setVolume(status.volume, false);
  $(".address").textContent = status.address ?? "";
  $(".hud-name").textContent = deviceName ?? "iPhone";
  $(".veil-detail").textContent = `Unlock ${deviceName ?? "your iPhone"} to keep mirroring.`;
  body.classList.toggle("paused", status.state === "paused");

  if (status.state === "idle") {
    swapText($(".status-text"), "Ready for AirPlay");
    if (phase === "live") closeScreen();
    releaseCapture();
    resetDecoder();
    setPhase("idle");
  } else if (phase === "idle") {
    showSettings(false);
    // Stay on "Connecting" until a frame is actually on screen.
    swapText($(".status-text"), `Connecting to ${deviceName ?? "iPhone"}`);
    // The screen zooms out of the AirPlay icon, so bring that page forward.
    selectSection("airplay");
    setPhase("connecting");
    wiggle();
    glint();
  }
}

/* ── Rename ──────────────────────────────────────────── */

const nameField = $(".name-field");
const nameHint = $(".name-hint");
const HINT = "Shown on your iPhone under Screen Mirroring";

function beginRename() {
  if (phase === "live") return;
  body.classList.add("renaming");
  nameHint.textContent = HINT;
  nameHint.classList.remove("error");
  nameField.value = receiverName;
  nameField.focus();
  nameField.select();
}

function endRename() {
  body.classList.remove("renaming");
  nameField.blur();
}

let committing = false;
async function commitRename() {
  if (!body.classList.contains("renaming") || committing) return;
  const value = nameField.value;
  if (value.trim() === receiverName) return endRename();
  committing = true;
  try {
    await request("rename", { name: value });
    endRename();
  } catch (message) {
    nameHint.textContent = String(message);
    nameHint.classList.add("error");
    nameField.classList.remove("shake");
    void nameField.offsetWidth;
    nameField.classList.add("shake");
    nameField.focus();
  } finally {
    committing = false;
  }
}

$(".name-button").addEventListener("click", beginRename);
nameField.addEventListener("keydown", (event) => {
  event.stopPropagation();
  if (event.key === "Enter") commitRename();
  if (event.key === "Escape") endRename();
});
nameField.addEventListener("blur", () => setTimeout(commitRename, 0));

/* ── Volume ──────────────────────────────────────────── */

const sliderEl = $(".slider");
const volumeEl = $(".volume");
const slider = { value: 0.8, dragging: false, beforeMute: 0.8, stretch: new Spring(1, 0.0005).configure(0.4, 0.6) };
slider.stretch.render = () => sliderEl.style.setProperty("--stretch", slider.stretch.value.toFixed(4));
const stretchItem = { springs: [slider.stretch], render: slider.stretch.render };

let sendQueued = false;
function setVolume(value, send = true, save = false) {
  slider.value = clamp(value, 0, 1);
  sliderEl.style.setProperty("--value", slider.value.toFixed(4));
  sliderEl.setAttribute("aria-valuenow", Math.round(slider.value * 100));
  volumeEl.dataset.level = slider.value === 0 ? 0 : slider.value < 0.34 ? 1 : slider.value < 0.67 ? 2 : 3;
  $(".speaker").setAttribute("aria-label", slider.value === 0 ? "Unmute" : "Mute");
  if (!send) return;
  const update = (save) => request("volume", { volume: slider.value, save }).catch(() => {});
  if (save) {
    update(true);
  } else if (!sendQueued) {
    // Coalesce drags into one update per frame.
    sendQueued = true;
    requestAnimationFrame(() => {
      sendQueued = false;
      update(false);
    });
  }
}

function sliderValueAt(clientX) {
  const r = sliderEl.getBoundingClientRect();
  const raw = (clientX - r.left - 11) / (r.width - 22);
  // Past either end the track stretches a little, like iOS sliders.
  const over = raw < 0 ? -raw : raw > 1 ? raw - 1 : 0;
  sliderEl.style.setProperty("--stretch-origin", raw < 0 ? "right" : "left");
  slider.stretch.target = 1 + Math.min(0.08, over * 0.12);
  run(stretchItem);
  return clamp(raw, 0, 1);
}

sliderEl.addEventListener("pointerdown", (event) => {
  slider.dragging = true;
  sliderEl.classList.add("dragging");
  sliderEl.setPointerCapture(event.pointerId);
  setVolume(sliderValueAt(event.clientX));
});
sliderEl.addEventListener("pointermove", (event) => {
  if (slider.dragging) setVolume(sliderValueAt(event.clientX));
});
const endDrag = () => {
  if (!slider.dragging) return;
  slider.dragging = false;
  sliderEl.classList.remove("dragging");
  slider.stretch.target = 1;
  run(stretchItem);
  setVolume(slider.value, true, true);
};
sliderEl.addEventListener("pointerup", endDrag);
sliderEl.addEventListener("pointercancel", endDrag);
sliderEl.addEventListener("keydown", (event) => {
  const step = { ArrowRight: 0.05, ArrowUp: 0.05, ArrowLeft: -0.05, ArrowDown: -0.05 }[event.key];
  if (step === undefined) return;
  event.preventDefault();
  event.stopPropagation();
  setVolume(slider.value + step, true, true);
});
$(".hud").addEventListener(
  "wheel",
  (event) => {
    event.preventDefault();
    setVolume(slider.value - Math.sign(event.deltaY) * 0.04, true, true);
    showHud(true);
  },
  { passive: false },
);

function toggleMute() {
  if (slider.value > 0) {
    slider.beforeMute = slider.value;
    setVolume(0, true, true);
  } else {
    setVolume(slider.beforeMute || 0.5, true, true);
  }
}

/* ── Sections ────────────────────────────────────────── */

let section = "airplay";

function selectSection(next) {
  if (section === next) return;
  section = next;
  body.dataset.section = next;
  for (const card of document.querySelectorAll(".section-card")) {
    card.setAttribute("aria-pressed", String(card.dataset.section === next));
  }
  syncInert();
  if (next !== "airplay") restTile();
  try {
    localStorage.setItem("section", next);
  } catch {}
}

/** Only what's in front can take focus and clicks. */
function syncInert() {
  const settingsOpen = body.classList.contains("settings-open");
  $(".home").inert = settingsOpen || section !== "airplay";
  $(".drop-page").inert = settingsOpen || section !== "drop";
  $(".sections").inert = settingsOpen;
  $(".settings").inert = !settingsOpen;
}
for (const card of document.querySelectorAll(".section-card")) {
  card.addEventListener("click", () => selectSection(card.dataset.section));
}

function setAliasNames(name) {
  for (const el of document.querySelectorAll(".receiver-alias")) el.textContent = name;
}

/* ── Settings ────────────────────────────────────────── */

const settingsSearch = $(".set-search input");
const settingsNav = [...document.querySelectorAll(".set-nav-item")];
const appearanceRadios = [...document.querySelectorAll('input[name="appearance"]')];
const requireCode = $(".require-code");
const settingsError = $(".set-error");

function showSettings(open) {
  if (open === body.classList.contains("settings-open")) return;
  if (open && phase === "live") return;
  body.classList.toggle("settings-open", open);
  syncInert();
  if (open) {
    restTile();
    $(".set-nav-item[aria-current]")?.focus({ preventScroll: true });
  } else {
    settingsSearch.value = "";
    filterSettings();
    $(".settings-button").focus({ preventScroll: true });
  }
}

function selectPane(name) {
  for (const item of settingsNav) {
    if (item.dataset.pane === name) item.setAttribute("aria-current", "page");
    else item.removeAttribute("aria-current");
  }
  for (const pane of document.querySelectorAll(".set-pane")) pane.hidden = pane.dataset.pane !== name;
  $(".set-title").textContent = settingsNav.find((item) => item.dataset.pane === name).getAttribute("aria-label");
  try {
    localStorage.setItem("settings-pane", name);
  } catch {}
}
for (const item of settingsNav) item.addEventListener("click", () => selectPane(item.dataset.pane));

function filterSettings() {
  const words = settingsSearch.value.trim().toLowerCase().split(/\s+/);
  let found = 0;
  for (const item of settingsNav) {
    item.hidden = !words.every((word) => item.dataset.keywords.split(" ").some((key) => key.startsWith(word)));
    if (!item.hidden) found++;
  }
  $(".set-no-results").hidden = found > 0;
}
settingsSearch.addEventListener("input", filterSettings);
settingsSearch.addEventListener("keydown", (event) => {
  if (event.key !== "Enter") return;
  const first = settingsNav.find((item) => !item.hidden);
  if (first) selectPane(first.dataset.pane);
});

function applyAppearance(value, animate = false) {
  const root = document.documentElement;
  const change = () => {
    if (value === "auto") delete root.dataset.theme;
    else root.dataset.theme = value;
  };
  for (const radio of appearanceRadios) radio.checked = radio.value === value;
  // The whole window cross-fades, as when macOS changes appearance.
  if (animate && document.startViewTransition && !reducedMotion.matches) document.startViewTransition(change);
  else change();
}
let savedAppearance = "auto";
for (const radio of appearanceRadios) {
  radio.addEventListener("change", () => {
    applyAppearance(radio.value, true);
    request("set-appearance", { appearance: radio.value })
      .then(() => (savedAppearance = radio.value))
      .catch(() => applyAppearance(savedAppearance, true));
  });
}

requireCode.addEventListener("change", () => {
  const on = requireCode.checked;
  settingsError.hidden = true;
  request("set-require-code", { on }).catch((error) => {
    requireCode.checked = !on;
    settingsError.textContent = String(error);
    settingsError.hidden = false;
  });
});

async function loadSettings() {
  const settings = await request("settings");
  savedAppearance = settings.appearance;
  applyAppearance(settings.appearance);
  requireCode.checked = settings.requireCode;
}

/* ── Connection code ─────────────────────────────────── */

let codeTimer = 0;
let focusBeforeCode = null;

function setCodeOpen(open) {
  if (open === body.classList.contains("code-open")) return;
  body.classList.toggle("code-open", open);
  $(".code-sheet").inert = !open;
  if (open) {
    releaseCapture();
    focusBeforeCode = document.activeElement;
    $('.code-sheet [data-action="code-cancel"]').focus({ preventScroll: true });
    // The code is only useful if it's seen, so bring the window back into view.
    if (appWindow) {
      appWindow.unminimize().catch(() => {});
      if (!document.hasFocus()) appWindow.requestUserAttention(2).catch(() => {});
    }
  } else {
    focusBeforeCode?.focus?.({ preventScroll: true });
    focusBeforeCode = null;
  }
}

function applyCode(event) {
  clearTimeout(codeTimer);
  const digits = $(".code-digits");
  const detail = $("#code-detail");
  if (event.wait) {
    digits.hidden = true;
    detail.classList.add("wrong");
    detail.textContent = `Too many wrong codes. Try again in ${event.wait} seconds.`;
    setCodeOpen(true);
    codeTimer = setTimeout(() => setCodeOpen(false), event.wait * 1000);
    return;
  }
  if (!event.code) {
    setCodeOpen(false);
    return;
  }
  const device = event.device || "your device";
  digits.hidden = false;
  [...event.code].forEach((digit, i) => (digits.children[i].textContent = digit));
  digits.setAttribute("aria-label", `Code ${[...event.code].join(" ")}`);
  detail.classList.toggle("wrong", Boolean(event.wrong));
  detail.textContent = event.wrong
    ? "That code wasn't right. Try again."
    : `Type this on ${device} when it asks for the AirPlay password.`;
  if (event.wrong) shake(digits);
  setCodeOpen(true);
  codeTimer = setTimeout(() => setCodeOpen(false), (event.expires ?? 120) * 1000);
}

/* ── Drop ────────────────────────────────────────────── */
// File sharing over LocalSend (src/drop.rs): files sent to this PC are saved
// to Downloads automatically; staged files go to a device picked under Nearby.

const files = { outbox: [], received: new Map(), active: new Set(), nearby: [], sends: new Map() };
const ICONS = {
  folder: '<svg viewBox="0 0 20 20"><path d="M2.5 5.5a1.5 1.5 0 0 1 1.5-1.5h3.6l1.8 2h6.6a1.5 1.5 0 0 1 1.5 1.5v7.5a1.5 1.5 0 0 1-1.5 1.5H4a1.5 1.5 0 0 1-1.5-1.5z"/></svg>',
  remove: '<svg viewBox="0 0 20 20"><path d="M6 6l8 8m0-8l-8 8"/></svg>',
  ring: '<svg class="ring" viewBox="0 0 68 68"><circle class="track" cx="34" cy="34" r="32"/><circle class="progress" cx="34" cy="34" r="32"/></svg>',
  phone: '<svg class="glyph" viewBox="0 0 24 24"><rect x="7" y="2.5" width="10" height="19" rx="2.6"/><path d="M10.5 5h3"/></svg>',
  computer: '<svg class="glyph" viewBox="0 0 24 24"><rect x="4.5" y="5" width="15" height="10" rx="1.6"/><path d="M2.5 18.5h19"/></svg>',
};
const SEND_LABELS = {
  waiting: "Waiting…",
  sending: "Sending…",
  done: "Sent",
  declined: "Declined",
  busy: "Busy",
  failed: "Couldn't send",
  unreachable: "Not reachable",
  cancelled: "Cancelled",
};
let messageCount = 0;

function formatSize(bytes) {
  if (!bytes) return "0 KB";
  const units = ["bytes", "KB", "MB", "GB"];
  const i = Math.min(units.length - 1, Math.floor(Math.log(bytes) / Math.log(1024)));
  const value = bytes / 1024 ** i;
  return `${i === 0 ? value : value.toFixed(value < 10 ? 1 : 0)} ${units[i]}`;
}

function element(tag, className, text) {
  const el = document.createElement(tag);
  if (className) el.className = className;
  if (text !== undefined) el.textContent = text;
  return el;
}

function iconButton(action, label, icon) {
  const button = element("button", "icon-btn");
  button.type = "button";
  button.dataset.fileAction = action;
  button.title = label;
  button.setAttribute("aria-label", label);
  button.innerHTML = icon;
  return button;
}

function shake(el) {
  el.classList.remove("shake");
  void el.offsetWidth;
  el.classList.add("shake");
}

function fileThumb(item) {
  const thumb = element("span", "thumb");
  if (item.kind === "message") {
    thumb.textContent = "TEXT";
  } else if (item.image && item.status === "done" && item.path && tauri?.core?.convertFileSrc) {
    const img = new Image();
    img.alt = "";
    img.src = tauri.core.convertFileSrc(item.path);
    thumb.append(img);
  } else {
    const dot = item.name.lastIndexOf(".");
    thumb.textContent = dot > 0 ? item.name.slice(dot + 1, dot + 5).toUpperCase() : "FILE";
  }
  return thumb;
}

function receivedRow(item) {
  const row = element("li", "file");
  row.dataset.id = item.id;
  const meta = element("span", "meta");
  meta.append(element("b", "", item.kind === "message" ? item.text.split("\n")[0] : item.name), element("small"));
  if (item.status === "incoming") {
    const bar = element("span", "bar");
    bar.append(element("i"));
    meta.append(bar);
  }
  row.append(fileThumb(item), meta);
  if (item.kind === "message") {
    row.title = "Copy";
  } else if (item.status === "done") {
    row.title = "Open";
    row.append(iconButton("reveal", "Show in Folder", ICONS.folder));
  }
  updateReceivedRow(row, item);
  return row;
}

function updateReceivedRow(row, item) {
  const small = row.querySelector("small");
  const from = item.from ? ` · ${item.from}` : "";
  const time = item.time ? ` · ${item.time}` : "";
  if (item.kind === "message") {
    small.textContent = `Message${from}${time}`;
  } else if (item.status === "incoming") {
    const known = item.total > 0;
    row.classList.toggle("indeterminate", !known);
    small.textContent = known ? `${formatSize(item.received)} of ${formatSize(item.total)}${from}` : "Receiving…";
    row.querySelector(".bar i")?.style.setProperty("--p", `${known ? Math.min(100, (item.received / item.total) * 100) : 0}%`);
  } else if (item.status === "failed") {
    small.textContent = "Couldn't receive";
  } else {
    small.textContent = `${formatSize(item.size)}${from}${time}`;
  }
}

function addReceived(item) {
  files.received.set(String(item.id), item);
  $(".received-list").prepend(receivedRow(item));
}

function replaceReceived(item) {
  const old = $(`.received-list [data-id="${item.id}"]`);
  files.received.set(String(item.id), item);
  const row = receivedRow(item);
  row.style.animation = "none";
  if (old) old.replaceWith(row);
  else $(".received-list").prepend(row);
}

function outboxRow(item) {
  const row = element("li", "file");
  row.dataset.id = item.id;
  const meta = element("span", "meta");
  meta.append(element("b", "", item.name), element("small", "", formatSize(item.size)));
  row.append(fileThumb({ ...item, status: "queued" }), meta, iconButton("remove", "Remove", ICONS.remove));
  return row;
}

function renderOutbox() {
  const list = $(".outbox-list");
  const ids = new Set(files.outbox.map((item) => String(item.id)));
  for (const row of [...list.children]) if (!ids.has(row.dataset.id)) row.remove();
  for (const item of files.outbox) {
    if (!list.querySelector(`[data-id="${item.id}"]`)) list.append(outboxRow(item));
  }
  $('[data-action="clear-outbox"]').hidden = files.outbox.length === 0;
}

function deviceBubble(device) {
  const bubble = element("button", "device");
  bubble.type = "button";
  bubble.setAttribute("role", "listitem");
  bubble.dataset.fingerprint = device.fingerprint;
  const avatar = element("span", "avatar");
  avatar.innerHTML = ICONS.ring + (device.deviceType === "mobile" ? ICONS.phone : ICONS.computer);
  bubble.append(avatar, element("b"), element("small"));
  bubble.addEventListener("click", () => sendTo(bubble.dataset.fingerprint, bubble));
  return bubble;
}

function updateBubble(bubble) {
  const device = files.nearby.find((d) => d.fingerprint === bubble.dataset.fingerprint);
  const send = files.sends.get(bubble.dataset.fingerprint);
  bubble.dataset.type = device?.deviceType ?? "desktop";
  bubble.dataset.state = send?.state ?? "";
  if (device) bubble.querySelector("b").textContent = device.alias;
  bubble.querySelector("small").textContent = send ? (SEND_LABELS[send.state] ?? "") : (device?.deviceModel ?? "");
  bubble.querySelector(".avatar").style.setProperty("--p", String(send?.progress ?? 0));
  const busy = send?.state === "waiting" || send?.state === "sending";
  bubble.title = busy ? "Click to cancel" : `Send to ${device?.alias ?? "device"}`;
}

function renderNearby() {
  const list = $(".nearby");
  const ids = new Set(files.nearby.map((d) => d.fingerprint));
  for (const bubble of [...list.children]) {
    if (!ids.has(bubble.dataset.fingerprint) && !files.sends.has(bubble.dataset.fingerprint)) bubble.remove();
  }
  for (const device of files.nearby) {
    let bubble = [...list.children].find((b) => b.dataset.fingerprint === device.fingerprint);
    if (!bubble) list.append((bubble = deviceBubble(device)));
  }
  for (const bubble of list.children) updateBubble(bubble);
}

async function sendTo(fingerprint, bubble) {
  const send = files.sends.get(fingerprint);
  if (send?.state === "waiting" || send?.state === "sending") {
    request("drop-cancel", { fingerprint }).catch(() => {});
    return;
  }
  if (!files.outbox.length) {
    shake(bubble);
    shake($(".dropzone"));
    return;
  }
  files.sends.set(fingerprint, { state: "waiting", progress: 0 });
  updateBubble(bubble);
  try {
    await request("drop-send", { fingerprint });
  } catch (message) {
    finishSend(fingerprint, "failed");
    showDropError(String(message));
  }
}

function finishSend(fingerprint, state) {
  files.sends.set(fingerprint, { state, progress: state === "done" ? 1 : 0 });
  renderNearby();
  setTimeout(() => {
    if (files.sends.get(fingerprint)?.state === state) {
      files.sends.delete(fingerprint);
      renderNearby();
    }
  }, 5000);
}

let errorTimer = 0;
function showDropError(text, sticky = false) {
  const error = $(".drop-error");
  error.textContent = text;
  error.hidden = !text;
  clearTimeout(errorTimer);
  if (text && !sticky) errorTimer = setTimeout(() => (error.hidden = true), 5000);
}

async function loadDrop() {
  const info = await request("drop-info");
  showDropError(info.error ?? "", true);
  files.outbox = info.outbox;
  renderOutbox();
  $(".received-list").replaceChildren();
  files.received.clear();
  for (const item of [...info.received].reverse()) addReceived({ ...item, status: "done" });
  files.nearby = info.nearby;
  renderNearby();
}

let bannerTimer = 0;
function showBanner(item) {
  const banner = $(".banner");
  const message = item.kind === "message";
  $(".banner-thumb").replaceChildren(fileThumb(item));
  $(".banner-title").textContent = `${message ? "Message" : "Received"}${item.from ? ` from ${item.from}` : ""}`;
  $(".banner-name").textContent = message ? item.text : item.name;
  $(".banner-button").textContent = message ? "Copy" : "Show";
  banner.dataset.id = item.id;
  banner.classList.add("show");
  clearTimeout(bannerTimer);
  bannerTimer = setTimeout(() => banner.classList.remove("show"), message ? 9000 : 5000);
  if (document.hidden || !document.hasFocus()) appWindow?.requestUserAttention(2).catch(() => {});
}

function copyText(text) {
  navigator.clipboard.writeText(text).catch(() => {});
}

function onDropEvent(event) {
  const time = () => new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  if (event.event === "incoming") {
    files.active.add(event.id);
    addReceived({ id: event.id, name: event.name, total: event.total, received: 0, from: event.from, status: "incoming" });
  } else if (event.event === "progress") {
    const item = files.received.get(String(event.id));
    const row = $(`.received-list [data-id="${event.id}"]`);
    if (item && row) {
      item.received = event.received;
      updateReceivedRow(row, item);
    }
  } else if (event.event === "received") {
    files.active.delete(event.item.id);
    const item = { ...event.item, status: "done", time: time() };
    replaceReceived(item);
    if (section !== "drop" || phase === "live") showBanner(item);
  } else if (event.event === "failed") {
    files.active.delete(event.id);
    const item = files.received.get(String(event.id));
    if (item) replaceReceived({ ...item, status: "failed" });
  } else if (event.event === "message") {
    const item = { id: `m${++messageCount}`, kind: "message", text: event.text, from: event.from, time: time() };
    addReceived(item);
    showBanner(item);
  } else if (event.event === "outbox") {
    files.outbox = event.items;
    renderOutbox();
  } else if (event.event === "nearby") {
    files.nearby = event.devices;
    renderNearby();
  } else if (event.event === "send") {
    if (event.state === "waiting" || event.state === "sending") {
      files.sends.set(event.fingerprint, { state: event.state, progress: event.progress ?? 0 });
      renderNearby();
    } else {
      finishSend(event.fingerprint, event.state);
    }
  }
  body.classList.toggle("receiving", files.active.size > 0);
}

async function chooseFiles() {
  const picked = await tauri?.dialog?.open({ multiple: true, title: "Choose Files to Send" }).catch(() => null);
  const paths = picked ? [picked].flat() : [];
  if (paths.length) request("drop-stage-paths", { paths }).catch(() => {});
}

const dropzone = $(".dropzone");
dropzone.addEventListener("click", chooseFiles);
dropzone.addEventListener("keydown", (event) => {
  if (event.key === "Enter" || event.key === " ") {
    event.preventDefault();
    chooseFiles();
  }
});

document.addEventListener("click", (event) => {
  const row = event.target.closest(".file");
  if (!row) return;
  const button = event.target.closest("[data-file-action]");
  const id = row.dataset.id;
  if (button?.dataset.fileAction === "remove") request("drop-remove", { item: Number(id) }).catch(() => {});
  else if (button?.dataset.fileAction === "reveal") request("drop-reveal", { item: Number(id) }).catch(() => {});
  else if (row.closest(".received-list")) {
    const item = files.received.get(id);
    if (item?.kind === "message") copyText(item.text);
    else if (item?.status === "done") request("drop-open", { item: Number(id) }).catch(() => {});
  }
});

// Files dragged onto the window arrive as paths from Tauri.
const webview = tauri?.webview?.getCurrentWebview?.();
webview?.onDragDropEvent(({ payload }) => {
  if (payload.type === "enter" || payload.type === "over") body.classList.add("dragging");
  else if (payload.type === "leave") body.classList.remove("dragging");
  else if (payload.type === "drop") {
    body.classList.remove("dragging");
    if (phase !== "live") selectSection("drop");
    if (payload.paths?.length) request("drop-stage-paths", { paths: payload.paths }).catch(() => {});
  }
});

/* ── Control the iPad with mouse and keyboard ─────────── */
// RustPlay poses as a Bluetooth mouse and keyboard (src/remote.rs). Input only
// flows while the mirrored screen holds the pointer lock, which needs this
// window in front; Esc or switching away releases every key and button.

const control = {
  on: false,
  state: "off",
  name: "",
  capturing: false,
  buttons: 0,
  dx: 0,
  dy: 0,
  wheel: 0,
  pan: 0,
  wheelRest: 0,
  panRest: 0,
  held: new Set(),
  keys: [],
};

// Ctrl acts as ⌘ (so Ctrl+C copies on the iPad), Alt as ⌥, right Ctrl as Control.
const MODIFIER_BITS = {
  ControlLeft: 0x08,
  ControlRight: 0x01,
  ShiftLeft: 0x02,
  ShiftRight: 0x20,
  AltLeft: 0x04,
  AltRight: 0x40,
  MetaLeft: 0x08,
  MetaRight: 0x80,
};

// KeyboardEvent.code → HID usage, by physical position, so the iPad's own
// keyboard layout setting decides what each key types.
const USAGES = (() => {
  const map = {};
  for (let i = 0; i < 26; i++) map[`Key${String.fromCharCode(65 + i)}`] = 0x04 + i;
  for (let i = 1; i <= 9; i++) map[`Digit${i}`] = 0x1d + i;
  map.Digit0 = 0x27;
  for (let i = 1; i <= 12; i++) map[`F${i}`] = 0x39 + i;
  for (let i = 1; i <= 9; i++) map[`Numpad${i}`] = 0x58 + i;
  Object.assign(map, {
    Enter: 0x28, Escape: 0x29, Backspace: 0x2a, Tab: 0x2b, Space: 0x2c, Minus: 0x2d, Equal: 0x2e,
    BracketLeft: 0x2f, BracketRight: 0x30, Backslash: 0x31, Semicolon: 0x33, Quote: 0x34, Backquote: 0x35,
    Comma: 0x36, Period: 0x37, Slash: 0x38, CapsLock: 0x39, PrintScreen: 0x46, ScrollLock: 0x47, Pause: 0x48,
    Insert: 0x49, Home: 0x4a, PageUp: 0x4b, Delete: 0x4c, End: 0x4d, PageDown: 0x4e, ArrowRight: 0x4f,
    ArrowLeft: 0x50, ArrowDown: 0x51, ArrowUp: 0x52, NumLock: 0x53, NumpadDivide: 0x54, NumpadMultiply: 0x55,
    NumpadSubtract: 0x56, NumpadAdd: 0x57, NumpadEnter: 0x58, Numpad0: 0x62, NumpadDecimal: 0x63,
    IntlBackslash: 0x64, ContextMenu: 0x65,
  });
  return map;
})();

const MOUSE_BUTTON_BITS = { 0: 0x01, 2: 0x02, 1: 0x04, 3: 0x08, 4: 0x10 };

/** Sends without waiting for a reply; these arrive many times a second. */
function sendRemote(type, payload) {
  if (socket?.readyState === WebSocket.OPEN) socket.send(JSON.stringify({ type, ...payload }));
}

let hintTimer = 0;
function showHint(text, ms = 0) {
  const hint = $(".control-hint");
  if (text) hint.textContent = text;
  hint.classList.toggle("show", Boolean(text));
  clearTimeout(hintTimer);
  if (text && ms) hintTimer = setTimeout(() => hint.classList.remove("show"), ms);
}

function applyRemote(status) {
  control.state = status.state;
  if (status.name) control.name = status.name;
  $(".control-toggle").dataset.state = status.state;
  if (status.state !== "off" && !control.on) setControl(true, false);
  // Switched off, but the iPad's Bluetooth link couldn't be dropped.
  if (status.state === "off" && status.detail) showHint(status.detail, 7000);
  if (!control.on) return;
  if (status.state === "advertising") {
    releaseCapture();
    showHint(`On your iPad, open Settings → Bluetooth and pair “${control.name}”`);
  } else if (status.state === "connected" && !control.capturing) {
    showHint("Click the screen to control your iPad", 4000);
  } else if (status.state === "error") {
    showHint(status.detail || "Bluetooth control isn't available on this PC", 6000);
    setControl(false, false);
  }
}

function setControl(on, tell = true) {
  control.on = on;
  $(".control-toggle").setAttribute("aria-pressed", String(on));
  if (tell) request("remote-enable", { on }).catch(() => {});
  if (!on) {
    releaseCapture();
    showHint("");
  }
}

function releaseCapture() {
  if (document.pointerLockElement) document.exitPointerLock();
  releaseAll();
}

/** Lets go of every button and key on the iPad side. */
function releaseAll() {
  const wasHolding = control.buttons || control.held.size;
  control.buttons = 0;
  control.held.clear();
  control.keys = [];
  control.dx = control.dy = control.wheel = control.pan = 0;
  if (wasHolding) {
    sendRemote("remote-mouse", { dx: 0, dy: 0, buttons: 0, wheel: 0, pan: 0 });
    sendRemote("remote-keys", { modifiers: 0, keys: [] });
  }
}

/** Sends input right away; the backend folds it into Bluetooth's pace. */
function flushMouse(always = false) {
  // Whole counts only; fractions carry over to the next event.
  const dx = Math.trunc(control.dx);
  const dy = Math.trunc(control.dy);
  const { wheel, pan, buttons } = control;
  if (!always && !dx && !dy && !wheel && !pan) return;
  control.dx -= dx;
  control.dy -= dy;
  control.wheel = control.pan = 0;
  sendRemote("remote-mouse", { dx, dy, buttons, wheel, pan });
}

function sendKeys() {
  let modifiers = 0;
  for (const code of control.held) modifiers |= MODIFIER_BITS[code] ?? 0;
  // AltGr arrives as ControlLeft + AltRight; don't let it press ⌘ too.
  if (control.held.has("AltRight") && control.held.has("ControlLeft")) modifiers &= ~0x08;
  sendRemote("remote-keys", { modifiers, keys: control.keys.slice(-6) });
}

screenEl.addEventListener("click", () => {
  if (!control.on || control.capturing) return;
  if (control.state !== "connected") return applyRemote({ state: control.state });
  // Raw movement skips Windows' pointer acceleration; the iPad applies its own.
  const locked = screenEl.requestPointerLock({ unadjustedMovement: true });
  locked?.catch?.(() => screenEl.requestPointerLock());
});

document.addEventListener("pointerlockchange", () => {
  const locked = document.pointerLockElement === screenEl;
  if (locked === control.capturing) return;
  control.capturing = locked;
  body.classList.toggle("capturing", locked);
  if (locked) {
    showHud(false);
    showHint("Controlling your iPad · press Esc to release", 2500);
  } else {
    releaseAll();
  }
});

// mousemove is held back until the next frame, which beats against Bluetooth's
// 15 ms rhythm and makes the pointer stutter; pointerrawupdate arrives as often
// as the mouse reports.
const MOVE_EVENT = "onpointerrawupdate" in window ? "pointerrawupdate" : "mousemove";
document.addEventListener(
  MOVE_EVENT,
  (event) => {
    if (!control.capturing) return;
    control.dx += event.movementX;
    control.dy += event.movementY;
    flushMouse();
  },
  true,
);

for (const type of ["mousedown", "mouseup"]) {
  document.addEventListener(
    type,
    (event) => {
      if (!control.capturing) return;
      event.preventDefault();
      event.stopPropagation();
      const bit = MOUSE_BUTTON_BITS[event.button] ?? 0;
      control.buttons = type === "mousedown" ? control.buttons | bit : control.buttons & ~bit;
      flushMouse(true);
    },
    true,
  );
}

document.addEventListener(
  "wheel",
  (event) => {
    if (!control.capturing) return;
    event.preventDefault();
    const scale = event.deltaMode === 1 ? 1 : 1 / 50;
    // Wheel up scrolls content up on the iPad, as on a Mac.
    control.wheelRest -= event.deltaY * scale;
    control.panRest += event.deltaX * scale;
    const wheel = Math.trunc(control.wheelRest);
    const pan = Math.trunc(control.panRest);
    control.wheelRest -= wheel;
    control.panRest -= pan;
    control.wheel += wheel;
    control.pan += pan;
    flushMouse();
  },
  { capture: true, passive: false },
);

function keyEvent(event, down) {
  if (!control.capturing) return;
  event.preventDefault();
  event.stopImmediatePropagation();
  if (event.repeat) return; // the iPad repeats held keys itself
  if (event.code in MODIFIER_BITS) {
    if (down) control.held.add(event.code);
    else control.held.delete(event.code);
  } else {
    const usage = USAGES[event.code];
    if (!usage) return;
    control.keys = control.keys.filter((key) => key !== usage);
    if (down) control.keys.push(usage);
  }
  sendKeys();
}
addEventListener("keydown", (event) => keyEvent(event, true), true);
addEventListener("keyup", (event) => keyEvent(event, false), true);

// Only while RustPlay is the window in front.
addEventListener("blur", releaseCapture);
document.addEventListener("visibilitychange", () => document.hidden && releaseCapture());

/* ── Video ───────────────────────────────────────────── */

let decoder = null;
let avcc = null;
let videoSize = { width: 1080, height: 1920 };
let waitingForKey = true;
let timestamp = 0;

const hex = (byte) => byte.toString(16).padStart(2, "0");
const sameBytes = (a, b) => a && b && a.length === b.length && a.every((v, i) => v === b[i]);

function configure(config, width, height) {
  const resized = width && height && (width !== videoSize.width || height !== videoSize.height);
  if (width && height) videoSize = { width, height };
  // Clients resend the same SPS/PPS around sleep; keep decoding if nothing changed.
  const unchanged = decoder?.state === "configured" && sameBytes(avcc, config);
  avcc = config;
  if (!unchanged) {
    if (decoder && decoder.state !== "closed") decoder.close();
    decoder = new VideoDecoder({ output: drawFrame, error: onDecoderError });
    decoder.configure({
      codec: `avc1.${hex(config[1])}${hex(config[2])}${hex(config[3])}`,
      description: config,
      optimizeForLatency: true,
    });
    waitingForKey = true;
  }
  if (resized && screenOpen) {
    screenEl.classList.add("reflow");
    setGeo(screenRect(), 0.5, 0.86);
  }
}

function onDecoderError(error) {
  console.warn("decoder error", error);
  if (avcc) setTimeout(() => configure(avcc), 0);
}

function decode(data, keyframe) {
  if (!decoder || decoder.state !== "configured") return;
  if (waitingForKey && !keyframe) return;
  waitingForKey = false;
  timestamp += 16667;
  decoder.decode(new EncodedVideoChunk({ type: keyframe ? "key" : "delta", timestamp, data }));
}

function drawFrame(frame) {
  const width = frame.displayWidth;
  const height = frame.displayHeight;
  if (canvas.width !== width || canvas.height !== height) {
    canvas.width = width;
    canvas.height = height;
  }
  if (width !== videoSize.width || height !== videoSize.height) {
    videoSize = { width, height };
    if (screenOpen) setGeo(screenRect(), 0.5, 0.86);
  }
  ctx.drawImage(frame, 0, 0);
  frame.close();
  screenEl.classList.remove("reflow");
  if (phase !== "live") {
    if (body.classList.contains("renaming")) endRename();
    setPhase("live");
    openScreen();
  }
}

function resetDecoder() {
  if (decoder && decoder.state !== "closed") decoder.close();
  decoder = null;
  avcc = null;
  waitingForKey = true;
}

/* ── Stream socket ───────────────────────────────────── */

async function streamUrl() {
  const fromQuery = new URLSearchParams(location.search).get("ws");
  if (fromQuery) return fromQuery;
  return tauri.core.invoke("stream_endpoint");
}

let socket = null;
let nextRequest = 1;
const pending = new Map();

/** Sends a command to the backend; resolves with its reply. */
function request(type, payload = {}) {
  if (!socket || socket.readyState !== WebSocket.OPEN) return Promise.reject("RustPlay isn't connected.");
  const id = nextRequest++;
  socket.send(JSON.stringify({ id, type, ...payload }));
  return new Promise((resolve, reject) => pending.set(id, { resolve, reject }));
}

async function connect() {
  const url = await streamUrl();
  let retry = 500;
  const open = () => {
    socket = new WebSocket(url);
    socket.binaryType = "arraybuffer";
    socket.onopen = () => {
      retry = 500;
      loadDrop().catch((error) => console.warn("drop info", error));
      loadSettings().catch((error) => console.warn("settings", error));
      request("remote-status").then(applyRemote).catch(() => {});
    };
    socket.onmessage = ({ data }) => {
      if (typeof data === "string") {
        const message = JSON.parse(data);
        if (message.type === "status") applyStatus(message);
        if (message.type === "drop") onDropEvent(message);
        if (message.type === "remote") applyRemote(message);
        if (message.type === "code") applyCode(message);
        if (message.type === "reply") {
          const waiter = pending.get(message.id);
          pending.delete(message.id);
          if (message.ok) waiter?.resolve(message);
          else waiter?.reject(message.error);
        }
        return;
      }
      const bytes = new Uint8Array(data);
      const view = new DataView(data);
      if (bytes[0] === 1) {
        configure(bytes.slice(6), view.getUint16(2, true), view.getUint16(4, true));
      } else if (bytes[0] === 2) {
        decode(bytes.subarray(2), bytes[1] === 1);
      }
    };
    socket.onclose = () => {
      for (const waiter of pending.values()) waiter.reject("RustPlay isn't connected.");
      pending.clear();
      setTimeout(open, retry);
      retry = Math.min(retry * 2, 5000);
    };
  };
  open();
}

/* ── HUD & window controls ───────────────────────────── */

let hudTimer = 0;
function showHud(visible) {
  clearTimeout(hudTimer);
  body.classList.toggle("hud-visible", visible);
  body.classList.toggle("hide-cursor", !visible && phase === "live" && body.classList.contains("fullscreen"));
  if (visible && phase === "live") {
    hudTimer = setTimeout(() => {
      if (!$(".hud").matches(":hover") && !slider.dragging) showHud(false);
    }, 2600);
  }
}
addEventListener("mousemove", () => phase === "live" && !control.capturing && showHud(true));
$(".hud").addEventListener("mouseleave", () => phase === "live" && showHud(true));

async function syncFullscreen() {
  const fullscreen = appWindow ? await appWindow.isFullscreen() : !!document.fullscreenElement;
  if (fullscreen === body.classList.contains("fullscreen")) return;
  body.classList.toggle("fullscreen", fullscreen);
  const label = fullscreen ? "Exit Full Screen" : "Enter Full Screen";
  for (const el of document.querySelectorAll('[data-action="fullscreen"]')) el.setAttribute("aria-label", label);
  if (screenOpen) setGeo(screenRect(), 0.45, 0.9);
}

async function toggleFullscreen(force) {
  if (appWindow) {
    await appWindow.setFullscreen(force ?? !(await appWindow.isFullscreen()));
  } else if (document.fullscreenElement || force === false) {
    await document.exitFullscreen?.();
  } else {
    await document.documentElement.requestFullscreen?.();
  }
  setTimeout(syncFullscreen, 60);
}

const actions = {
  close: () => appWindow?.close(),
  minimize: () => appWindow?.minimize(),
  fullscreen: () => toggleFullscreen(),
  stop: () => request("stop").catch(() => {}),
  mute: () => toggleMute(),
  control: () => setControl(!control.on),
  settings: () => showSettings(true),
  "settings-close": () => showSettings(false),
  "code-cancel": () => {
    clearTimeout(codeTimer);
    setCodeOpen(false);
    request("code-cancel").catch(() => {});
  },
  "open-folder": () => request("drop-open-folder").catch(() => {}),
  scan: () => {
    request("drop-scan").catch(() => {});
    const button = $('[data-action="scan"]');
    button.textContent = "Looking…";
    setTimeout(() => (button.textContent = "Refresh"), 2500);
  },
  "clear-outbox": () => {
    for (const item of files.outbox) request("drop-remove", { item: item.id }).catch(() => {});
  },
  "banner-show": () => {
    const banner = $(".banner");
    banner.classList.remove("show");
    const item = files.received.get(banner.dataset.id);
    if (item?.kind === "message") copyText(item.text);
    else if (phase === "live") request("drop-reveal", { item: Number(banner.dataset.id) }).catch(() => {});
    else selectSection("drop");
  },
};
document.addEventListener("click", (event) => {
  const button = event.target.closest("[data-action]");
  if (button) actions[button.dataset.action]?.();
});
// Double-clicks belong to the iPad while mouse control is on.
screenEl.addEventListener("dblclick", () => !control.on && toggleFullscreen());
addEventListener("keydown", (event) => {
  if (event.key === "Escape" && body.classList.contains("code-open")) return actions["code-cancel"]();
  if (event.key === "," && event.ctrlKey) return showSettings(!body.classList.contains("settings-open"));
  if (event.key === "Escape" && body.classList.contains("settings-open")) {
    // Esc clears the search first, like the search field in System Settings.
    if (settingsSearch.value) {
      settingsSearch.value = "";
      filterSettings();
    } else {
      showSettings(false);
    }
    return;
  }
  if (event.key === "Escape" && body.classList.contains("fullscreen")) toggleFullscreen(false);
  if (phase !== "live" || event.ctrlKey || event.metaKey) return;
  if (event.key === "f" || event.key === "F") toggleFullscreen();
  if (event.key === "ArrowUp" || event.key === "ArrowDown") {
    setVolume(slider.value + (event.key === "ArrowUp" ? 0.05 : -0.05), true, true);
    showHud(true);
  }
  if (event.key === "m" || event.key === "M") toggleMute();
});

addEventListener("blur", () => body.classList.add("inactive"));
addEventListener("focus", () => body.classList.remove("inactive"));

tilt.render();
try {
  if (localStorage.getItem("section") === "drop") selectSection("drop");
  const pane = localStorage.getItem("settings-pane");
  if (settingsNav.some((item) => item.dataset.pane === pane)) selectPane(pane);
} catch {}
setAliasNames(receiverName);
introduce();
connect().catch((error) => console.error("could not open the stream", error));

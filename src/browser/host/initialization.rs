pub const USER_INPUT_INITIALIZATION_SCRIPT: &str = r#"
(() => {
  const marker = "__devmanagerBrowser";
  if (window[marker]) return;

  const MAX_CONSOLE = 200;
  const MAX_NETWORK = 300;
  const MAX_PERFORMANCE = 500;
  const MAX_BODY_BYTES = 64 * 1024;
  const REDACTED = "[redacted]";
  let pendingSecretTicket = null;
  const MAX_MASKED_SECRET_ELEMENTS = 32;
  let maskedSecretElementRefs = [];
  const state = {
    console: [],
    network: [],
    performance: [],
    bodies: new Map(),
    sequence: 0,
    requestSequence: 0,
    inflightRequests: 0,
    lastNetworkActivityAt: 0,
    tracing: false,
    traceStartedAt: 0,
    annotationActive: false,
    secretTainted: false,
  };

  const boundedPush = (list, value, maximum) => {
    if (state.secretTainted) return;
    list.push(value);
    while (list.length > maximum) list.shift();
  };
  const SECRET_KEY_SUFFIXES = ["token", "secret", "cookie"];
  const SECRET_KEY_PREFIXES = ["authorization", "password", "passwd"];
  const secretKey = (key) => {
    const normalized = String(key).replace(/[^a-z0-9]/gi, "").toLowerCase();
    return ["apikey", "privatekey"].includes(normalized) ||
      SECRET_KEY_SUFFIXES.some((suffix) => normalized === suffix || normalized.endsWith(suffix)) ||
      SECRET_KEY_PREFIXES.some((prefix) => normalized === prefix || normalized.startsWith(prefix));
  };
  const redactStructured = (value) => {
    const text = String(value ?? "");
    if (!/^[\s]*[\[{]/.test(text)) return text;
    try {
      const visit = (current) => {
        if (Array.isArray(current)) return current.map(visit);
        if (!current || typeof current !== "object") return current;
        return Object.fromEntries(Object.entries(current).map(([key, nested]) =>
          [key, secretKey(key) ? REDACTED : visit(nested)]
        ));
      };
      return JSON.stringify(visit(JSON.parse(text)));
    } catch (_) {
      return text;
    }
  };
  const redact = (value) => redactStructured(value)
    .replace(/\bBasic\s+[A-Za-z0-9._~+\/=\-]+/gi, `Basic ${REDACTED}`)
    .replace(/\bBearer\s+[A-Za-z0-9._~+\/=\-]+/gi, `Bearer ${REDACTED}`)
    .replace(/("([^"\\]*(?:\\.[^"\\]*)*)"\s*:\s*")((?:\\.|[^"\\])*)(")/g,
      (match, prefix, key, _secret, suffix) => secretKey(key) ? `${prefix}${REDACTED}${suffix}` : match)
    .replace(/((?:[a-z0-9_-]*(?:token|secret|cookie)|(?:authorization|password|passwd)[a-z0-9_-]*|(?:api|private)[_-]?key)\s*[:=]\s*)([^\s,;]+)/gi, `$1${REDACTED}`)
    .slice(0, 4000);
  const safeUrl = (value) => {
    try {
      const parsed = new URL(String(value), location.href);
      for (const key of [...parsed.searchParams.keys()]) {
        if (/authorization|cookie|password|token|secret|key/i.test(key)) {
          parsed.searchParams.set(key, REDACTED);
        }
      }
      parsed.username = "";
      parsed.password = "";
      return redact(parsed.toString()).slice(0, 4000);
    } catch (_) {
      return redact(value);
    }
  };
  const now = () => Math.max(0, Math.round(performance.now()));
  const rawPostMessage = window.ipc.postMessage.bind(window.ipc);
  const valueFreeControlMessage = (body) => {
    if (typeof body !== "string") return false;
    try {
      const parsed = JSON.parse(body);
      if (!parsed || Array.isArray(parsed) || typeof parsed !== "object") return false;
      const keys = Object.keys(parsed).sort();
      if (parsed.type === "domMutation" || parsed.type === "annotationCanceled") {
        return keys.length === 1 && keys[0] === "type";
      }
      return parsed.type === "userInput" &&
        keys.length === 2 && keys[0] === "kind" && keys[1] === "type" &&
        ["pointer", "keyboard", "textInput"].includes(parsed.kind);
    } catch (_) {
      return false;
    }
  };
  window.ipc.postMessage = (body) => {
    if (!state.secretTainted || valueFreeControlMessage(body)) rawPostMessage(body);
  };

  const reportInput = (kind) => (event) => {
    if (!event.isTrusted || state.annotationActive) return;
    window.ipc.postMessage(JSON.stringify({ type: "userInput", kind }));
  };
  window.addEventListener("pointerdown", reportInput("pointer"), true);
  window.addEventListener("keydown", reportInput("keyboard"), true);
  window.addEventListener("input", reportInput("textInput"), true);

  const annotationOwnedNodes = new WeakSet();
  const annotationOwnedNode = (node) => {
    if (!(node instanceof Element)) return false;
    if (annotationOwnedNodes.has(node)) return true;
    let ancestor = node.parentElement;
    while (ancestor) {
      if (annotationOwnedNodes.has(ancestor)) return true;
      ancestor = ancestor.parentElement;
    }
    return false;
  };
  const annotationOwnedMutation = (record) => {
    if (annotationOwnedNode(record.target)) return true;
    const changedNodes = [
      ...Array.from(record.addedNodes || []),
      ...Array.from(record.removedNodes || []),
    ];
    return changedNodes.length > 0 && changedNodes.every(annotationOwnedNode);
  };
  const registerMaskedSecretElement = (element) => {
    const retained = [];
    let found = false;
    for (const reference of maskedSecretElementRefs) {
      const current = reference.deref();
      if (!current || current.isConnected !== true) continue;
      if (current === element) found = true;
      retained.push(reference);
    }
    if (!found && retained.length >= MAX_MASKED_SECRET_ELEMENTS) {
      maskedSecretElementRefs = retained;
      return false;
    }
    if (!found) retained.push(new WeakRef(element));
    maskedSecretElementRefs = retained;
    return true;
  };
  const enforceSecretMask = () => {
    if (!state.secretTainted) return;
    const retained = [];
    for (const reference of maskedSecretElementRefs) {
      const element = reference.deref();
      if (!element || element.isConnected !== true) continue;
      retained.push(reference);
      if (element.style?.getPropertyValue?.("-webkit-text-security") !== "disc" ||
          element.style?.getPropertyPriority?.("-webkit-text-security") !== "important") {
        element.style?.setProperty?.("-webkit-text-security", "disc", "important");
      }
    }
    maskedSecretElementRefs = retained;
  };
  const isMaskedSecretElement = (element) =>
    maskedSecretElementRefs.some((reference) => reference.deref() === element);
  let mutationTimer = null;
  const mutationObserver = new MutationObserver((records) => {
    enforceSecretMask();
    if (!records.some((record) => !annotationOwnedMutation(record))) return;
    if (mutationTimer !== null) return;
    mutationTimer = setTimeout(() => {
      mutationTimer = null;
      window.ipc.postMessage(JSON.stringify({ type: "domMutation" }));
    }, 50);
  });
  mutationObserver.observe(document, {
    subtree: true,
    childList: true,
    attributes: true,
    characterData: true,
  });

  for (const level of ["debug", "info", "log", "warn", "error"]) {
    const original = console[level]?.bind(console);
    if (!original) continue;
    console[level] = (...args) => {
      if (state.secretTainted) return original(...args);
      boundedPush(state.console, {
        sequence: ++state.sequence,
        level,
        message: redact(args.map((arg) => {
          try { return redact(typeof arg === "string" ? arg : JSON.stringify(arg)); }
          catch (_) { return redact(String(arg)); }
        }).join(" ")).slice(0, 4000),
        timestampMs: Date.now(),
      }, MAX_CONSOLE);
      return original(...args);
    };
  }
  window.addEventListener("error", (event) => {
    if (state.secretTainted) return;
    boundedPush(state.console, {
      sequence: ++state.sequence,
      level: "error",
      message: redact(event.message || "runtime error"),
      timestampMs: Date.now(),
    }, MAX_CONSOLE);
  });
  window.addEventListener("unhandledrejection", (event) => {
    if (state.secretTainted) return;
    boundedPush(state.console, {
      sequence: ++state.sequence,
      level: "error",
      message: redact(event.reason || "unhandled rejection"),
      timestampMs: Date.now(),
    }, MAX_CONSOLE);
  });

  const beginRequest = (url, method) => {
    state.inflightRequests += 1;
    state.lastNetworkActivityAt = now();
    if (state.secretTainted) {
      return { requestId: null, startedAt: now(), suppressed: true };
    }
    return {
      requestId: `request-${++state.requestSequence}`,
      url: safeUrl(url),
      method: String(method || "GET").toUpperCase().slice(0, 32),
      status: null,
      failed: false,
      bodyAvailable: false,
      durationMs: null,
      startedAt: now(),
    };
  };
  const finishRequest = (entry, status, failed) => {
    if (state.secretTainted || entry.suppressed) {
      state.inflightRequests = Math.max(0, state.inflightRequests - 1);
      state.lastNetworkActivityAt = now();
      return;
    }
    entry.status = Number.isFinite(status) ? status : null;
    entry.failed = Boolean(failed);
    entry.durationMs = Math.max(0, now() - entry.startedAt);
    delete entry.startedAt;
    state.inflightRequests = Math.max(0, state.inflightRequests - 1);
    state.lastNetworkActivityAt = now();
    boundedPush(state.network, entry, MAX_NETWORK);
  };
  const captureBody = async (entry, response) => {
    try {
      if (state.secretTainted || entry.suppressed) return;
      if (new URL(response.url).origin !== location.origin) return;
      const contentType = response.headers.get("content-type") || "";
      if (!/json|text|javascript|xml|form/i.test(contentType)) return;
      const body = await response.clone().text();
      if (state.secretTainted) return;
      if (new TextEncoder().encode(body).byteLength > MAX_BODY_BYTES) return;
      state.bodies.set(entry.requestId, redact(body));
      entry.bodyAvailable = true;
      while (state.bodies.size > 32) state.bodies.delete(state.bodies.keys().next().value);
    } catch (_) {}
  };
  const originalFetch = window.fetch?.bind(window);
  if (originalFetch) {
    window.fetch = async (...args) => {
      const request = args[0];
      const options = args[1] || {};
      const entry = beginRequest(request?.url || request, options.method || request?.method);
      try {
        const response = await originalFetch(...args);
        await captureBody(entry, response);
        finishRequest(entry, response.status, !response.ok);
        return response;
      } catch (error) {
        finishRequest(entry, null, true);
        throw error;
      }
    };
  }

  const xhrOpen = XMLHttpRequest.prototype.open;
  const xhrSend = XMLHttpRequest.prototype.send;
  XMLHttpRequest.prototype.open = function(method, url, ...rest) {
    const result = xhrOpen.call(this, method, url, ...rest);
    this.__devmanagerRequestMetadata = state.secretTainted ? null : { url, method };
    return result;
  };
  XMLHttpRequest.prototype.send = function(...args) {
    const metadata = this.__devmanagerRequestMetadata || { url: location.href, method: "GET" };
    const entry = beginRequest(metadata.url, metadata.method);
    let finished = false;
    const finish = () => {
      if (finished) return;
      finished = true;
      try {
        if (state.secretTainted || entry.suppressed) {
          finishRequest(entry, null, false);
          return;
        }
        const contentType = this.getResponseHeader("content-type") || "";
        if (new URL(entry.url).origin === location.origin && /json|text|javascript|xml|form/i.test(contentType)) {
          const body = typeof this.responseText === "string" ? this.responseText : "";
          if (new TextEncoder().encode(body).byteLength <= MAX_BODY_BYTES) {
            state.bodies.set(entry.requestId, redact(body));
            entry.bodyAvailable = true;
          }
        }
      } catch (_) {}
      finishRequest(entry, this.status || null, this.status === 0 || this.status >= 400);
    };
    try {
      this.addEventListener("loadend", finish, { once: true });
      return xhrSend.apply(this, args);
    } catch (error) {
      if (!finished) {
        finished = true;
        finishRequest(entry, null, true);
      }
      throw error;
    }
  };

  try {
    const performanceObserver = new PerformanceObserver((list) => {
      if (state.secretTainted) return;
      for (const entry of list.getEntries()) {
        boundedPush(state.performance, {
          name: safeUrl(entry.name),
          entryType: entry.entryType,
          startTime: Math.max(0, Math.round(entry.startTime)),
          duration: Math.max(0, Math.round(entry.duration)),
        }, MAX_PERFORMANCE);
      }
    });
    performanceObserver.observe({ entryTypes: ["navigation", "resource", "longtask", "paint"] });
  } catch (_) {}

  const implicitRole = (element) => {
    const tag = element.tagName?.toLowerCase();
    if (tag === "button") return "button";
    if (tag === "a" && element.hasAttribute("href")) return "link";
    if (tag === "textarea") return "textbox";
    if (tag === "select") return "combobox";
    if (tag === "h1" || tag === "h2" || tag === "h3" || tag === "h4" || tag === "h5" || tag === "h6") return "heading";
    if (tag === "input") {
      const type = (element.getAttribute("type") || "text").toLowerCase();
      if (type === "checkbox") return "checkbox";
      if (type === "radio") return "radio";
      if (["button", "submit", "reset"].includes(type)) return "button";
      return "textbox";
    }
    return null;
  };
  const roleOf = (element) => element.getAttribute?.("role") || implicitRole(element);
  const labelOf = (element) => {
    const id = element.id;
    const explicit = id ? document.querySelector(`label[for="${CSS.escape(id)}"]`) : null;
    return redact(explicit?.innerText || element.closest?.("label")?.innerText || "").slice(0, 1000) || null;
  };
  const isPasswordElement = (element) =>
    isMaskedSecretElement(element) ||
    String(element?.getAttribute?.("type") || "").toLowerCase() === "password";
  const nameOf = (element) => {
    const valueFallback = isPasswordElement(element) ? "" : element.getAttribute?.("value");
    return redact(
      element.getAttribute?.("aria-label") ||
      element.getAttribute?.("alt") ||
      element.getAttribute?.("title") ||
      labelOf(element) ||
      element.innerText ||
      valueFallback ||
      ""
    ).trim().slice(0, 1000) || null;
  };
  const annotationSemantic = (value) => {
    if (value === null || value === undefined) return null;
    const normalized = redact(String(value))
      .replace(/[\u0000-\u001f\u007f-\u009f]/g, " ")
      .trim()
      .slice(0, 1000);
    return normalized || null;
  };
  const clampedAnnotationBounds = (bounds) => {
    const viewportWidth = Math.max(1, Math.round(window.innerWidth));
    const viewportHeight = Math.max(1, Math.round(window.innerHeight));
    const rawLeft = Number.isFinite(bounds.left) ? bounds.left : bounds.x;
    const rawTop = Number.isFinite(bounds.top) ? bounds.top : bounds.y;
    const rawRight = Number.isFinite(bounds.right) ? bounds.right : rawLeft + bounds.width;
    const rawBottom = Number.isFinite(bounds.bottom) ? bounds.bottom : rawTop + bounds.height;
    const left = Math.max(0, Math.min(viewportWidth - 1, Math.floor(rawLeft)));
    const top = Math.max(0, Math.min(viewportHeight - 1, Math.floor(rawTop)));
    const right = Math.max(left + 1, Math.min(viewportWidth, Math.ceil(rawRight)));
    const bottom = Math.max(top + 1, Math.min(viewportHeight, Math.ceil(rawBottom)));
    return { x: left, y: top, width: right - left, height: bottom - top };
  };
  const isVisible = (element) => {
    if (!(element instanceof Element)) return false;
    const bounds = element.getBoundingClientRect();
    const style = getComputedStyle(element);
    return bounds.width > 0 && bounds.height > 0 && style.display !== "none" && style.visibility !== "hidden" && Number(style.opacity) !== 0;
  };
  const cssFallbacks = (element) => {
    const selectors = [];
    if (element.id) selectors.push(`#${CSS.escape(element.id)}`);
    const name = element.getAttribute?.("name");
    if (name) selectors.push(`${element.tagName.toLowerCase()}[name="${CSS.escape(name)}"]`);
    const parent = element.parentElement;
    if (parent) {
      const siblings = [...parent.children].filter((child) => child.tagName === element.tagName);
      selectors.push(`${element.tagName.toLowerCase()}:nth-of-type(${siblings.indexOf(element) + 1})`);
    }
    return selectors.slice(0, 4);
  };
  const annotationStyleKeys = [
    "display", "position", "color", "backgroundColor", "fontFamily", "fontSize",
    "fontWeight", "border", "borderRadius", "padding", "margin", "opacity", "visibility",
  ];
  const annotationComputedStyle = (element) => {
    if (!element) return {};
    const computedStyle = getComputedStyle(element);
    return Object.fromEntries(annotationStyleKeys.map((key) => [key, redact(computedStyle[key] || "").slice(0, 256)]));
  };
  let annotationSession = null;
  const annotationOverlayMutation = (operation) => operation();
  const annotationCleanup = (notify) => {
    const session = annotationSession;
    if (!session) return;
    annotationSession = null;
    state.annotationActive = false;
    session.overlay.removeEventListener("pointerdown", session.pointerDown);
    session.overlay.removeEventListener("pointermove", session.pointerMove);
    session.overlay.removeEventListener("pointerup", session.pointerUp);
    session.overlay.removeEventListener("pointercancel", session.pointerCancel);
    window.removeEventListener("keydown", session.keyDown, true);
    window.removeEventListener("resize", session.resize, true);
    annotationOverlayMutation(() => session.overlay.remove());
    if (notify) window.ipc.postMessage(JSON.stringify({ type: "annotationCanceled" }));
  };
  const annotationElementAt = (overlay, x, y) => annotationOverlayMutation(() => {
    const previous = overlay.style.display;
    overlay.style.display = "none";
    const element = document.elementFromPoint(x, y);
    overlay.style.display = previous;
    return element;
  });
  const annotationStart = (context) => {
    annotationCleanup(false);
    const revision = Number(context?.revision);
    const url = String(context?.url || "");
    if (!Number.isSafeInteger(revision) || revision < 0 || !url) return false;

    const overlay = document.createElement("div");
    overlay.setAttribute("data-devmanager-annotation-overlay", "true");
    Object.assign(overlay.style, {
      position: "fixed", inset: "0", zIndex: "2147483647", cursor: "crosshair",
      background: "rgba(59, 130, 246, 0.04)", pointerEvents: "auto", userSelect: "none",
    });
    const selection = document.createElement("div");
    selection.setAttribute("data-devmanager-annotation-selection", "true");
    annotationOwnedNodes.add(overlay);
    annotationOwnedNodes.add(selection);
    Object.assign(selection.style, {
      position: "fixed", display: "none", border: "2px solid #3b82f6",
      background: "rgba(59, 130, 246, 0.16)", pointerEvents: "none",
      boxSizing: "border-box",
    });
    overlay.appendChild(selection);

    let start = null;
    let dragged = false;
    const updateSelection = (x, y) => {
      if (!start) return;
      const left = Math.max(0, Math.min(start.x, x));
      const top = Math.max(0, Math.min(start.y, y));
      const right = Math.min(window.innerWidth, Math.max(start.x, x));
      const bottom = Math.min(window.innerHeight, Math.max(start.y, y));
      dragged = Math.abs(x - start.x) >= 4 || Math.abs(y - start.y) >= 4;
      annotationOverlayMutation(() => Object.assign(selection.style, {
        display: "block", left: `${left}px`, top: `${top}px`,
        width: `${Math.max(1, right - left)}px`, height: `${Math.max(1, bottom - top)}px`,
      }));
    };
    const candidateContext = () => ({
      url,
      revision,
      viewport: {
        width: Math.max(1, Math.round(window.innerWidth)),
        height: Math.max(1, Math.round(window.innerHeight)),
        scalePercent: Math.max(25, Math.min(500, Math.round((window.devicePixelRatio || 1) * 100))),
      },
    });
    const finalize = (x, y) => {
      if (!start) return;
      let candidate;
      if (dragged) {
        const left = Math.max(0, Math.min(start.x, x));
        const top = Math.max(0, Math.min(start.y, y));
        const right = Math.min(window.innerWidth, Math.max(start.x, x));
        const bottom = Math.min(window.innerHeight, Math.max(start.y, y));
        candidate = {
          kind: "region",
          ...candidateContext(),
          locator: { accessibilityRole: null, accessibilityName: null, testId: null, cssSelectors: [] },
          bounds: clampedAnnotationBounds({ x: left, y: top, width: right - left, height: bottom - top }),
          computedStyles: {},
        };
      } else {
        const element = annotationElementAt(overlay, x, y);
        if (!(element instanceof Element) || !isVisible(element)) {
          annotationCleanup(true);
          return;
        }
        const bounds = element.getBoundingClientRect();
        candidate = {
          kind: "element",
          ...candidateContext(),
          locator: {
            accessibilityRole: annotationSemantic(roleOf(element)),
            accessibilityName: annotationSemantic(nameOf(element)),
            testId: annotationSemantic(element.getAttribute?.("data-testid")),
            cssSelectors: cssFallbacks(element),
          },
          bounds: clampedAnnotationBounds(bounds),
          computedStyles: annotationComputedStyle(element),
        };
      }
      annotationCleanup(false);
      window.ipc.postMessage(JSON.stringify({ type: "annotationCandidate", candidate }));
    };
    const pointerDown = (event) => {
      if (!event.isTrusted) return;
      event.preventDefault(); event.stopPropagation();
      start = { x: event.clientX, y: event.clientY };
      dragged = false;
      overlay.setPointerCapture?.(event.pointerId);
      updateSelection(event.clientX, event.clientY);
    };
    const pointerMove = (event) => {
      if (!start || !event.isTrusted) return;
      event.preventDefault(); event.stopPropagation();
      updateSelection(event.clientX, event.clientY);
    };
    const pointerUp = (event) => {
      if (!start || !event.isTrusted) return;
      event.preventDefault(); event.stopPropagation();
      updateSelection(event.clientX, event.clientY);
      finalize(event.clientX, event.clientY);
    };
    const pointerCancel = (event) => {
      if (event.isTrusted) annotationCleanup(true);
    };
    const keyDown = (event) => {
      if (event.isTrusted && event.key === "Escape") {
        event.preventDefault(); event.stopPropagation(); annotationCleanup(true);
      }
    };
    const resize = () => annotationCleanup(true);
    annotationSession = { overlay, pointerDown, pointerMove, pointerUp, pointerCancel, keyDown, resize };
    state.annotationActive = true;
    overlay.addEventListener("pointerdown", pointerDown);
    overlay.addEventListener("pointermove", pointerMove);
    overlay.addEventListener("pointerup", pointerUp);
    overlay.addEventListener("pointercancel", pointerCancel);
    window.addEventListener("keydown", keyDown, true);
    window.addEventListener("resize", resize, true);
    annotationOverlayMutation(() => document.body.appendChild(overlay));
    return true;
  };
  const resolveTarget = (target) => {
    const locator = target?.locator || target?.elementRef?.locator || {};
    if (locator.testId) {
      const element = document.querySelector(`[data-testid="${CSS.escape(locator.testId)}"]`);
      if (element) return element;
    }
    if (locator.accessibilityRole && locator.accessibilityName) {
      const element = [...document.querySelectorAll("*")].find((candidate) =>
        roleOf(candidate) === locator.accessibilityRole && nameOf(candidate) === locator.accessibilityName
      );
      if (element) return element;
    }
    for (const selector of locator.cssSelectors || []) {
      try {
        const element = document.querySelector(selector);
        if (element) return element;
      } catch (_) {}
    }
    if (target?.coordinates) return document.elementFromPoint(target.coordinates.x, target.coordinates.y);
    return null;
  };
  const dispatchValueEvents = (element) => {
    element.dispatchEvent(new Event("input", { bubbles: true }));
    element.dispatchEvent(new Event("change", { bubbles: true }));
  };
  const applyAction = (action) => {
    const element = resolveTarget(action.target || action.source);
    if (!element && action.operation !== "scroll" && action.operation !== "keypress") throw new Error("element_not_found");
    switch (action.operation) {
      case "click": element.click(); break;
      case "hover": element.dispatchEvent(new MouseEvent("mousemove", { bubbles: true })); break;
      case "focus": element.focus(); break;
      case "type": element.focus(); element.value = String(action.text ?? ""); dispatchValueEvents(element); break;
      case "clear": element.focus(); element.value = ""; dispatchValueEvents(element); break;
      case "select": {
        const values = new Set(action.values || []);
        for (const option of element.options || []) option.selected = values.has(option.value);
        dispatchValueEvents(element);
        break;
      }
      case "keypress": {
        const destination = resolveTarget(action.target) || document.activeElement || document.body;
        destination.dispatchEvent(new KeyboardEvent("keydown", { key: action.key, bubbles: true }));
        destination.dispatchEvent(new KeyboardEvent("keyup", { key: action.key, bubbles: true }));
        break;
      }
      case "scroll": {
        const destination = resolveTarget(action.target) || window;
        destination.scrollBy?.({ left: action.deltaX || 0, top: action.deltaY || 0, behavior: "instant" });
        break;
      }
      case "dragDrop": {
        const destination = resolveTarget(action.destination);
        if (!destination) throw new Error("element_not_found");
        const transfer = new DataTransfer();
        element.dispatchEvent(new DragEvent("dragstart", { bubbles: true, dataTransfer: transfer }));
        destination.dispatchEvent(new DragEvent("drop", { bubbles: true, dataTransfer: transfer }));
        element.dispatchEvent(new DragEvent("dragend", { bubbles: true, dataTransfer: transfer }));
        break;
      }
      default: throw new Error("unsupported_action");
    }
  };
  const checkWait = (condition, elapsed) => {
    switch (condition.type) {
      case "duration": return elapsed >= condition.durationMs;
      case "url": return condition.exact ? location.href === condition.value : location.href.includes(condition.value);
      case "load": return document.readyState === "complete";
      case "networkIdle": return state.inflightRequests === 0 && now() - state.lastNetworkActivityAt >= 500;
      case "title": return condition.exact ? document.title === condition.value : document.title.includes(condition.value);
      case "elementPresent": return Boolean(resolveTarget(condition.target));
      case "elementAbsent": return !resolveTarget(condition.target);
      case "elementVisible": return isVisible(resolveTarget(condition.target));
      case "elementHidden": return !isVisible(resolveTarget(condition.target));
      case "elementValue": {
        const element = resolveTarget(condition.target);
        if (!element) return false;
        const value = "value" in element ? element.value : element.getAttribute?.("value");
        return String(value ?? "") === String(condition.value ?? "");
      }
      case "textPresent": return (document.body?.innerText || "").includes(condition.text);
      case "textAbsent": return !(document.body?.innerText || "").includes(condition.text);
      case "javaScript": {
        const predicate = String(condition.predicate || "");
        if (predicate.length > 512 || /(?:fetch|XMLHttpRequest|eval|Function|import|require|cookie|localStorage|sessionStorage|\bnew\b|=[^=])/i.test(predicate)) return false;
        try { return Boolean(Function(`"use strict"; return !!(${predicate});`)()); }
        catch (_) { return false; }
      }
      default: return false;
    }
  };

  const runtimeTarget = (element) => {
    const form = element?.closest?.("form");
    if (state.secretTainted) {
      const inputType = String(element?.getAttribute?.("type") || "").toLowerCase();
      const autocomplete = String(element?.getAttribute?.("autocomplete") || "")
        .toLowerCase().split(/\s+/).at(-1);
      let formAction = null;
      try { formAction = form?.action ? new URL(form.action, location.href).origin : null; }
      catch (_) {}
      return {
        originUrl: location.origin,
        role: element ? implicitRole(element) : null,
        name: null,
        inputType: ["text", "password", "email", "tel", "url", "number", "search"].includes(inputType) ? inputType : null,
        autocomplete: ["current-password", "new-password", "one-time-code", "username", "email", "off"].includes(autocomplete) ? autocomplete : null,
        formAction,
        permission: null,
      };
    }
    return {
      originUrl: location.origin,
      role: element ? roleOf(element) : null,
      name: element ? nameOf(element) : null,
      inputType: element?.getAttribute?.("type") || null,
      autocomplete: element?.getAttribute?.("autocomplete") || null,
      formAction: form?.action ? safeUrl(form.action) : null,
      permission: null,
    };
  };
  const secretRiskSignature = (element) => JSON.stringify(runtimeTarget(element));
  const clearContentTelemetry = () => {
    state.console.length = 0;
    state.network.length = 0;
    state.performance.length = 0;
    state.bodies.clear();
    state.tracing = false;
    state.traceStartedAt = 0;
  };
  const markSecretTainted = () => {
    if (state.secretTainted) return;
    state.secretTainted = true;
    clearContentTelemetry();
    annotationCleanup(false);
  };
  const requireContentTelemetry = () => {
    if (state.secretTainted) throw new Error("secret_tainted_document");
  };

  window[marker] = {
    snapshot: () => {
      requireContentTelemetry();
      const useful = "a,button,input,select,textarea,[role],[data-testid],h1,h2,h3,h4,h5,h6,p,li,summary";
      return [...document.querySelectorAll(useful)].filter(isVisible).slice(0, 2000).map((element) => {
        const bounds = element.getBoundingClientRect();
        const inputType = element.getAttribute?.("type");
        const password = isPasswordElement(element);
        const value = "value" in element ? (password ? REDACTED : redact(element.value)) : null;
        return {
          role: roleOf(element),
          name: nameOf(element),
          label: labelOf(element),
          text: redact(element.innerText || "").trim().slice(0, 2000) || null,
          testId: element.getAttribute?.("data-testid"),
          cssSelectors: cssFallbacks(element),
          bounds: { x: Math.round(bounds.x), y: Math.round(bounds.y), width: Math.round(bounds.width), height: Math.round(bounds.height) },
          enabled: !(element.disabled || element.getAttribute?.("aria-disabled") === "true"),
          checked: "checked" in element ? Boolean(element.checked) : null,
          value,
          inputType,
          interactive: Boolean(element.matches?.("a,button,input,select,textarea,[role],[data-testid]")),
        };
      });
    },
    inspectTargets: (actions) => actions.flatMap((action) => {
      const elements = action.operation === "dragDrop"
        ? [resolveTarget(action.source), resolveTarget(action.destination)]
        : action.operation === "keypress" && !action.target
          ? [document.activeElement]
          : [resolveTarget(action.target || action.source)];
      return elements.map(runtimeTarget);
    }),
    inspectSecretTarget: (target, token) => {
      pendingSecretTicket = null;
      const normalizedToken = String(token ?? "");
      if (typeof WeakRef !== "function" || !normalizedToken || normalizedToken.length > 256 || /[\u0000-\u001f\u007f]/.test(normalizedToken)) return null;
      const element = resolveTarget(target);
      if (!element) return null;
      const inspectedTarget = runtimeTarget(element);
      pendingSecretTicket = Object.freeze({
        token: normalizedToken,
        element: new WeakRef(element),
        signature: JSON.stringify(inspectedTarget),
      });
      return inspectedTarget;
    },
    typeSecret: (token, value) => {
      const ticket = pendingSecretTicket;
      pendingSecretTicket = null;
      if (!ticket || ticket.token !== String(token ?? "")) {
        markSecretTainted();
        throw new Error("element_not_found");
      }
      const element = ticket.element.deref();
      if (!element) {
        markSecretTainted();
        throw new Error("element_not_found");
      }
      const connected = element.isConnected === true;
      const currentSignature = secretRiskSignature(element);
      markSecretTainted();
      if (!connected || ticket.signature !== currentSignature) {
        throw new Error("target_changed");
      }
      if (!registerMaskedSecretElement(element)) throw new Error("target_changed");
      element.style?.setProperty?.("-webkit-text-security", "disc", "important");
      element.focus();
      element.value = String(value ?? "");
      dispatchValueEvents(element);
      enforceSecretMask();
      return { completedActions: 1 };
    },
    act: (actions) => {
      let completedActions = 0;
      for (const action of actions) {
        applyAction(action);
        completedActions += 1;
      }
      return { completedActions };
    },
    wait: async (condition, timeoutMs) => {
      const started = now();
      for (;;) {
        const elapsedMs = now() - started;
        if (checkWait(condition, elapsedMs)) return { matched: true, elapsedMs };
        if (elapsedMs >= timeoutMs) return { matched: false, elapsedMs };
        await new Promise((resolve) => setTimeout(resolve, 25));
      }
    },
    console: (operation) => {
      requireContentTelemetry();
      if (operation === "clear") { state.console.length = 0; return []; }
      return state.console.slice();
    },
    network: (operation, requestId) => {
      requireContentTelemetry();
      if (operation === "clear") { state.network.length = 0; state.bodies.clear(); return []; }
      if (operation === "body") return state.bodies.has(requestId) ? { available: true, body: state.bodies.get(requestId) } : { available: false };
      return state.network.slice();
    },
    performance: (operation) => {
      requireContentTelemetry();
      if (operation === "traceStart") { state.tracing = true; state.traceStartedAt = now(); state.performance.length = 0; return { tracing: true }; }
      if (operation === "traceStop") { state.tracing = false; return { tracing: false, trace: state.performance.slice() }; }
      const navigation = performance.getEntriesByType("navigation")[0];
      return { navigation: navigation?.toJSON?.() || {}, entries: state.performance.slice() };
    },
    markUpload: (target, token) => {
      const element = resolveTarget(target);
      if (!element || element.tagName?.toLowerCase() !== "input" || String(element.type).toLowerCase() !== "file") return false;
      element.setAttribute("data-devmanager-upload", token);
      return true;
    },
    annotation: {
      start: (context) => {
        requireContentTelemetry();
        return annotationStart(context);
      },
      cancel: () => annotationCleanup(false),
      active: () => Boolean(annotationSession),
    },
    secretTainted: () => state.secretTainted,
  };
})();
"#;

pub fn browser_user_input_initialization_script() -> &'static str {
    USER_INPUT_INITIALIZATION_SCRIPT
}

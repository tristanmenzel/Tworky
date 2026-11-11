(function (mode, payload) {
  const NAME_HINT = /user|email|login|account/i;
  const TEXT_TYPES = ["text", "email", "tel", "search", ""];

  const isVisible = (el) =>
    el.type !== "hidden" &&
    !el.disabled &&
    (el.offsetParent !== null || el.getClientRects().length > 0);

  const collect = (root, out, onRoot) => {
    if (onRoot) onRoot(root);
    for (const el of root.querySelectorAll("*")) {
      if (el.tagName === "INPUT") out.push(el);
      if (el.shadowRoot) collect(el.shadowRoot, out, onRoot);
    }
  };

  const isUsernameCandidate = (el) => {
    const type = el.type;
    const autocomplete = el.getAttribute("autocomplete") || "";
    if (/(^|\s)username(\s|$)/i.test(autocomplete)) return TEXT_TYPES.includes(type);
    if (type === "email") return true;
    if (!["text", "email", "tel"].includes(type)) return false;
    return NAME_HINT.test(el.name || "") || NAME_HINT.test(el.id || "") || NAME_HINT.test(autocomplete);
  };

  const find = (onRoot) => {
    const inputs = [];
    collect(document, inputs, onRoot);
    const visible = inputs.filter(isVisible);
    const password = visible.find((el) => el.type === "password") || null;
    const candidates = visible.filter(isUsernameCandidate);
    let username = null;
    if (password) {
      const index = visible.indexOf(password);
      const before = candidates.filter((el) => visible.indexOf(el) < index);
      const sameForm = before.filter((el) => password.form && el.form === password.form);
      username = sameForm.pop() || before.pop() || null;
    } else {
      username = candidates[0] || null;
    }
    return { username, password };
  };

  if (mode === "fill") {
    const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value").set;
    const found = find();
    let last = null;
    const filled = [];
    const fill = (name, el, value) => {
      if (!el || typeof value !== "string") return;
      setter.call(el, value);
      el.dispatchEvent(new Event("input", { bubbles: true, composed: true }));
      el.dispatchEvent(new Event("change", { bubbles: true, composed: true }));
      last = el;
      filled.push(name);
    };
    fill("username", found.username, payload.username);
    fill("password", found.password, payload.password);
    if (last) last.focus();

    const SUBMIT_TEXT = /sign in|log in|login|next|continue|submit/i;
    const findButton = () => {
      const buttons = [];
      const gather = (root) => {
        for (const el of root.querySelectorAll("*")) {
          if (el.matches("button, input[type=submit], [role=button]")) buttons.push(el);
          if (el.shadowRoot) gather(el.shadowRoot);
        }
      };
      gather(document);
      const visible = buttons.filter(isVisible);
      return (
        visible.find((el) => el.matches("button[type=submit], input[type=submit]")) ||
        visible.find((el) => SUBMIT_TEXT.test(el.value || el.textContent || "")) ||
        null
      );
    };
    let submitted = false;
    if (payload.submit === true && last) {
      if (last.form) {
        try {
          last.form.requestSubmit();
          submitted = true;
        } catch (_) {}
      }
      if (!submitted) {
        const button = findButton();
        if (button) {
          button.click();
          submitted = true;
        }
      }
    }
    return { filled, submitted };
  }

  if (window.top !== window) return;
  const report = window.__tworkyLogin;
  if (typeof report !== "function") return;
  const post = (message) => {
    try {
      report(JSON.stringify(message));
    } catch (_) {}
  };

  let reported = "";
  const observed = new WeakSet();
  const observer = new MutationObserver(() => schedule());
  const watch = (root) => {
    if (observed.has(root)) return;
    observed.add(root);
    observer.observe(root, {
      childList: true,
      subtree: true,
      attributes: true,
      attributeFilter: ["type", "style", "class", "hidden", "disabled", "name", "id", "autocomplete"],
    });
  };

  const check = () => {
    const found = find(watch);
    const fields = [];
    if (found.username) fields.push("username");
    if (found.password) fields.push("password");
    const key = fields.join(",");
    if (key === reported) return;
    reported = key;
    post(fields.length ? { type: "login-form", fields } : { type: "login-form-gone" });
  };

  let timer = 0;
  const schedule = () => {
    clearTimeout(timer);
    timer = setTimeout(check, 250);
  };

  watch(document);
  schedule();
  window.addEventListener("load", schedule);
  window.addEventListener("pageshow", schedule);
  window.addEventListener(
    "keydown",
    (event) => {
      if (event.ctrlKey && event.shiftKey && !event.altKey && !event.metaKey && event.key.toLowerCase() === "l") {
        event.preventDefault();
        post({ type: "fill-request" });
      }
    },
    true,
  );
})

// Injected into the app by signalbox's Components tab, next to the tap fix.
// Adds where each inspected component is used (the stack of the JSX that
// created it, as `signalboxStack`, which signalbox maps to a source line), and
// keeps prop edits through re-renders. React DevTools edits a prop by
// overriding it until the parent renders again, and the parent's next render
// brings its own value back. Here an edit becomes a pin, kept by one of two
// strategies (`globalThis.__signalboxPinMode`, which signalbox sets first):
//
// a. Re-apply: after each commit, a pinned prop the parent reset is overridden
//    again. Works for any component; costs one more render after each parent
//    render, and the parent's value may show for a frame.
// b. Wrap: the component's type is swapped, through React Refresh's type
//    resolution (the way Fast Refresh swaps edited code), for a wrapper that
//    hands the pinned instance its pinned props on every render. Nothing to
//    re-apply, so nothing to flash. Function and forwardRef components only;
//    editing the component's own file remounts it.
//
// A pin is kept by the element's place in the tree (each fiber's index and key
// from the root) and type, so it holds when the component remounts in the
// same place, e.g. after navigating away and back. A reload drops it.
// Idempotent; run again after a reload and after each connection.
(() => {
  const hook = globalThis.__REACT_DEVTOOLS_GLOBAL_HOOK__;
  if (hook == null || hook.renderers == null) return 'unsupported';
  const state =
    hook.__signalboxPins ||
    (hook.__signalboxPins = {pins: [], wrapped: new Map(), byType: new Map(), seq: 0});
  state.mode = globalThis.__signalboxPinMode || 'a';

  const same = (a, b) => {
    if (a === b) return true;
    try {
      return JSON.stringify(a) === JSON.stringify(b);
    } catch (e) {
      return false;
    }
  };
  const getIn = (obj, path) => path.reduce((o, k) => (o == null ? undefined : o[k]), obj);
  const setIn = (obj, path, i, value) => {
    if (i >= path.length) return value;
    const copy = Array.isArray(obj) ? obj.slice() : Object.assign({}, obj);
    copy[path[i]] = setIn(obj == null ? undefined : obj[path[i]], path, i + 1, value);
    return copy;
  };

  // A fiber's place: its root and the index:key of each fiber below it.
  const placeOf = (fiber) => {
    const steps = [];
    let f = fiber;
    for (; f != null && f.tag !== 3; f = f.return) steps.push(f.index + ':' + (f.key == null ? '' : f.key));
    return f == null ? null : {root: f.stateNode, at: steps.reverse().join('/')};
  };
  const descend = (root, at) => {
    let f = root.current;
    for (const step of at === '' ? [] : at.split('/')) {
      let c = f.child;
      while (c != null && c.index + ':' + (c.key == null ? '' : c.key) !== step) c = c.sibling;
      if (c == null) return null;
      f = c;
    }
    return f;
  };
  // The mounted fiber at a pin's place, if it still has the pinned type.
  const fiberAt = (pin) => {
    const f = descend(pin.root, pin.at);
    return f != null && f.elementType === pin.type ? f : null;
  };
  // The mounted fiber for a React DevTools element id.
  const fiberFor = (rendererID, ri, id) => {
    const frames = ri.getPathForElement && ri.getPathForElement(id);
    if (!frames || frames.length < 2) return null;
    const at = frames
      .slice(1)
      .map((p) => p.index + ':' + (p.key == null ? '' : p.key))
      .join('/');
    for (const root of hook.getFiberRoots(rendererID)) {
      const f = descend(root, at);
      if (f != null) return f;
    }
    return null;
  };
  const pinsAt = (place, type) =>
    place == null ? [] : state.pins.filter((p) => p.root === place.root && p.at === place.at && p.type === type);
  const withPins = (props, pins) => pins.reduce((acc, p) => setIn(acc, p.path, 0, p.value), props);

  // a. After each commit, override again what the parent reset.
  const reapply = () => {
    if (state.mode !== 'a') return;
    for (const pin of state.pins) {
      const f = fiberAt(pin);
      if (f == null || same(getIn(f.memoizedProps, pin.path), pin.value)) continue;
      const now = Date.now();
      // A component that keeps losing it (a render loop): give up on it.
      pin.recent = (pin.recent || []).filter((t) => now - t < 1000).concat(now);
      if (pin.recent.length > 30) {
        pin.dead = true;
        continue;
      }
      hook.renderers.get(pin.rendererID).overrideProps(f, pin.path, pin.value);
    }
    state.pins = state.pins.filter((p) => !p.dead);
  };
  if (!hook.onCommitFiberRoot.__signalbox) {
    const onCommit = hook.onCommitFiberRoot;
    const wrapped = function () {
      const r = onCommit.apply(this, arguments);
      try {
        reapply();
      } catch (e) {}
      return r;
    };
    wrapped.__signalbox = true;
    hook.onCommitFiberRoot = wrapped;
  }

  // b. Swap the component's type for one that applies its pins.
  const refresh = globalThis.__ReactRefresh;
  const wrapType = (renderer, fiber) => {
    if (refresh == null || typeof renderer.getCurrentFiber !== 'function') return 'no React Refresh in this app';
    let base;
    if (fiber.tag === 0 || fiber.tag === 11) base = fiber.elementType;
    else if (fiber.tag === 15) base = fiber.elementType && fiber.elementType.type;
    else return 'only function components can be pinned this way';
    if (state.wrapped.has(base)) return null;
    if (refresh.getFamilyByType(base) == null) refresh.register(base, 'signalbox:' + state.seq++);
    const family = refresh.getFamilyByType(base);
    if (family == null) return 'React Refresh won’t track this component';
    let real = family.current;
    const pinned = (props) => withPins(props, pinsAt(placeOf(renderer.getCurrentFiber()), fiber.elementType));
    let wrapper;
    if (fiber.tag === 11) {
      wrapper = {
        $$typeof: base.$$typeof,
        render: (props, ref) => real.render(pinned(props), ref),
        displayName: base.displayName || (base.render && base.render.name),
      };
    } else {
      wrapper = function (props, secondArg) {
        return real(pinned(props), secondArg);
      };
      Object.defineProperty(wrapper, 'name', {value: real.name});
      if (real.displayName) wrapper.displayName = real.displayName;
    }
    // Everything that resolves the type (React, React Refresh) gets the
    // wrapper; new code from Fast Refresh goes inside it.
    Object.defineProperty(family, 'current', {
      configurable: true,
      get: () => wrapper,
      set: (v) => {
        if (v !== wrapper) real = v;
      },
    });
    state.wrapped.set(base, wrapper);
    state.byType.set(wrapper, family);
    // React resolves types only once given a handler (Fast Refresh sets one on
    // its first update); it must also know the wrapper, which it now renders.
    if (!renderer.setRefreshHandler.__signalbox) {
      const set = renderer.setRefreshHandler;
      const patched = function (handler) {
        return set.call(this, handler == null ? null : (t) => handler(t) || state.byType.get(t));
      };
      patched.__signalbox = true;
      renderer.setRefreshHandler = patched;
    }
    renderer.setRefreshHandler((t) => refresh.getFamilyByType(t));
    return null;
  };

  const pin = (rendererID, ri, id, path, value) => {
    const fiber = fiberFor(rendererID, ri, id);
    const place = fiber && placeOf(fiber);
    if (place == null) return 'couldn’t find this component in the tree';
    if (state.mode === 'b') {
      const why = wrapType(hook.renderers.get(rendererID), fiber);
      if (why) return why;
    }
    const key = path.join('.');
    state.pins = state.pins.filter(
      (p) => !(p.root === place.root && p.at === place.at && p.type === fiber.elementType && p.key === key),
    );
    state.pins.push({rendererID, root: place.root, at: place.at, type: fiber.elementType, path, key, value, id});
    return null;
  };

  // Pin every prop edit, by wrapping React DevTools' edit.
  const list = hook.rendererInterfaces instanceof Map ? [...hook.rendererInterfaces] : Object.entries(hook.rendererInterfaces || {});
  for (const [rid, ri] of list) {
    const edit = ri && ri.overrideValueAtPath;
    if (typeof edit !== 'function' || edit.__signalbox) continue;
    const rendererID = Number(rid);
    const wrapped = function (type, id, hookID, path, value) {
      if (type === 'props' && state.mode !== 'off') {
        try {
          state.lastError = pin(rendererID, ri, id, path, value);
        } catch (e) {
          state.lastError = String(e);
        }
      }
      return edit.apply(this, arguments);
    };
    wrapped.__signalbox = true;
    ri.overrideValueAtPath = wrapped;
  }

  // Where each inspected element was created: React keeps the stack of the
  // JSX call on the fiber (React 19, dev builds).
  for (const [rid, ri] of list) {
    const inspect = ri && ri.inspectElement;
    if (typeof inspect !== 'function' || inspect.__signalbox) continue;
    const rendererID = Number(rid);
    const wrapped = function (requestID, id) {
      const res = inspect.apply(this, arguments);
      try {
        if (res && res.type === 'full-data' && res.value) {
          const fiber = fiberFor(rendererID, ri, id);
          const stack = fiber && fiber._debugStack;
          if (stack && typeof stack.stack === 'string') res.value.signalboxStack = stack.stack;
        }
      } catch (e) {}
      return res;
    };
    wrapped.__signalbox = true;
    ri.inspectElement = wrapped;
  }

  // Stop keeping one edit, once it's been put back to the value it had: the
  // parent's value applies again on its next render.
  state.unpin = (id, path) => {
    const key = path.join('.');
    for (const [rid, ri] of list) {
      const fiber = fiberFor(Number(rid), ri, id);
      const place = fiber && placeOf(fiber);
      if (place) state.pins = state.pins.filter((p) => !(p.root === place.root && p.at === place.at && p.key === key));
    }
    return 'ok';
  };
  return 'ok';
})()

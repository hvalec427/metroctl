// Injected into the app by signalbox's Components tab (`s`, select by tapping).
// React Native's selection overlay hit-tests the tap with each renderer's
// `getInspectorDataForViewAtPoint` and hands the result to React DevTools'
// agent. Two things break that on Fabric, fixed here by wrapping the hit test:
//
// 1. Pager pages (react-native-pager-view, so tab views): every page shares the
//    same frame in the shadow tree, so the native hit test lands in the topmost
//    page, often not the one on screen, and selects its full-screen container.
//    A hit inside a page that isn't shown is redone inside the shown page.
// 2. Text: Fabric creates a text's public instance lazily, after the agent filed
//    the text under its host instance, so the agent can't resolve the hit and
//    the overlay drops it. The hit is handed over as the nearest host instance
//    the agent knows: the hit component's own host, else an ancestor's.
//
// Idempotent; run again after a reload.
(() => {
  const VERSION = 3;
  const hook = globalThis.__REACT_DEVTOOLS_GLOBAL_HOOK__;
  if (hook == null || hook.renderers == null) return 'unsupported';

  const known = (target) => {
    const agent = hook.reactDevtoolsAgent;
    return target != null && agent != null && agent.getIDForHostInstance(target) != null;
  };
  const firstHost = (fiber) => {
    for (let c = fiber; c != null; c = c.child) {
      if (c.tag === 5 || c.tag === 6) return c;
    }
    return null;
  };
  const resolve = (fiber) => {
    for (let f = firstHost(fiber) || fiber; f != null; f = f.return) {
      const node = f.stateNode;
      if (node == null || typeof node !== 'object') continue;
      const pub = node.canonical && node.canonical.publicInstance;
      if (known(pub)) return pub;
      if (known(node)) return node;
    }
    return null;
  };
  const same = (a, b) => a != null && (a === b || a === b.alternate);

  // React Native's own accessor, which creates a public instance on demand.
  let toPublicFn;
  const toPublic = (fiber) => {
    if (toPublicFn === undefined) {
      toPublicFn = null;
      try {
        for (const [id, m] of __r.getModules()) {
          if ((m.verboseName || '').endsWith('Libraries/ReactNative/RendererImplementation.js')) {
            toPublicFn = __r(id).getPublicInstanceFromInternalInstanceHandle || null;
            break;
          }
        }
      } catch (e) {}
    }
    try {
      return toPublicFn ? toPublicFn(fiber) : null;
    } catch (e) {
      return null;
    }
  };

  // The page shown instead of the one `fiber` is in, if `fiber` sits in a pager
  // page that isn't on screen. The shown page comes from the nearest
  // `navigationState.index` above the pager (react-native-tab-view).
  const shownPageInstead = (fiber) => {
    for (let p = fiber; p != null && p.return != null; p = p.return) {
      const pager = p.return;
      if (pager.tag !== 5 || !/ViewPager|PagerView/.test(String(pager.type))) continue;
      const pages = [];
      for (let c = pager.child; c != null; c = c.sibling) pages.push(c);
      const hitIndex = pages.findIndex((c) => same(c, p));
      let shown = null;
      for (let a = pager; a != null && shown == null; a = a.return) {
        const ns = a.memoizedProps && a.memoizedProps.navigationState;
        if (ns && typeof ns.index === 'number' && Array.isArray(ns.routes) && ns.routes.length === pages.length) {
          shown = ns.index;
        }
      }
      if (shown == null || hitIndex < 0 || hitIndex === shown) return null;
      return pages[shown] || null;
    }
    return null;
  };

  const patch = (renderer) => {
    const config = renderer && renderer.rendererConfig;
    const current = config && config.getInspectorDataForViewAtPoint;
    if (typeof current !== 'function' || current.__signalbox === VERSION) return;
    const hit = current.__signalboxOriginal || current;
    const wrapped = function (view, x, y, callback) {
      const self = this;
      const finish = (data) => {
        if (data && data.closestInstance != null && !known(data.closestPublicInstance)) {
          const target = resolve(data.closestInstance);
          if (target != null) data = Object.assign({}, data, {closestPublicInstance: target});
        }
        return callback(data);
      };
      return hit.call(self, view, x, y, (data) => {
        const page = data && shownPageInstead(data.closestInstance);
        const host = page && firstHost(page);
        const node = host && host.stateNode && host.stateNode.node;
        const pagePublic = host && toPublic(host);
        const fabric = globalThis.nativeFabricUIManager;
        if (node == null || pagePublic == null || fabric == null) return finish(data);
        fabric.measure(node, (_x, _y, _w, _h, pageX, pageY) => {
          let done = false;
          const fallback = setTimeout(() => done || ((done = true), finish(data)), 300);
          hit.call(self, pagePublic, x - pageX, y - pageY, (inner) => {
            if (done) return true;
            done = true;
            clearTimeout(fallback);
            return finish(inner && inner.hierarchy && inner.hierarchy.length > 0 ? inner : data);
          });
        });
        return true;
      });
    };
    wrapped.__signalbox = VERSION;
    wrapped.__signalboxOriginal = hit;
    config.getInspectorDataForViewAtPoint = wrapped;
  };
  hook.renderers.forEach(patch);
  if (!hook.__signalboxTapFix) {
    hook.__signalboxTapFix = true;
    hook.on('renderer', (e) => patch(e && e.renderer));
  }
  return 'ok';
})()

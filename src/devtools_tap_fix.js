// Injected into the app by signalbox's Components tab (`s`, select by tapping),
// right after it connects to the React DevTools backend. React Native's
// selection overlay hit-tests the tap with each renderer's
// `getInspectorDataForViewAtPoint` and hands the result to React DevTools'
// agent. Things that break that on Fabric, fixed here:
//
// 0. A stale agent: AppContainer keeps the agent that existed when it first
//    rendered, and only follows newer ones if there was none then. After a
//    reload the backend connects before AppContainer renders, so a later
//    connection (signalbox reattaching) leaves the overlay on a dead agent and
//    `s` does nothing. AppContainer's state is set to the current agent.
//
// 1. Pager pages (react-native-pager-view, so tab views): every page shares the
//    same frame in the shadow tree, so the native hit test lands in the topmost
//    page, often not the one on screen, and selects its full-screen container.
//    A hit inside a page that isn't shown is redone inside the shown page.
// 2. Text: Fabric creates a text's public instance lazily, after the agent filed
//    the text under its host instance, so the agent can't resolve the hit and
//    the overlay drops it. The hit is handed over as the nearest host instance
//    the agent knows: the hit component's own host, else an ancestor's.
// 3. Text highlights: React Native outlines an element through its public
//    instance and skips it when there is none, so selected text got no
//    outline. Highlighted host instances get theirs created first.
//
// Idempotent; run again after a reload and after each connection.
(() => {
  const VERSION = 5;
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

  // 0. Point AppContainer (and so its overlay) at the current agent.
  const followCurrentAgent = () => {
    const current = hook.reactDevtoolsAgent;
    if (current == null || !hook.getFiberRoots) return;
    for (const root of hook.getFiberRoots(1)) {
      const queue = [[root.current, 0]];
      while (queue.length) {
        const [f, depth] = queue.shift();
        if (f == null || depth > 40) continue;
        if (f.type && f.type.name === 'ReactDevToolsOverlay') {
          const stale = f.memoizedProps.reactDevToolsAgent;
          if (stale === current) return;
          // AppContainer: the component that rendered the overlay; its
          // `useState` hook holding the stale agent.
          for (let owner = f.return; owner != null; owner = owner.return) {
            for (let hookState = owner.memoizedState; hookState != null; hookState = hookState.next) {
              if (hookState.memoizedState === stale && hookState.queue && typeof hookState.queue.dispatch === 'function') {
                hookState.queue.dispatch(current);
                return;
              }
            }
          }
          return;
        }
        queue.push([f.child, depth + 1], [f.sibling, depth]);
      }
    }
  };

  // 3. Public instances for the host instances React DevTools highlights.
  const withPublicInstances = (renderers) => {
    if (renderers == null) return;
    const list = renderers instanceof Map ? [...renderers.values()] : Object.values(renderers);
    for (const ri of list) {
      const find = ri && ri.findHostInstancesForElementID;
      if (typeof find !== 'function' || find.__signalbox) continue;
      const wrapped = function (id) {
        const nodes = find.apply(this, arguments);
        for (const node of nodes || []) {
          const canonical = node && node.canonical;
          if (canonical && canonical.publicInstance == null && canonical.internalInstanceHandle) {
            toPublic(canonical.internalInstanceHandle);
          }
        }
        return nodes;
      };
      wrapped.__signalbox = true;
      ri.findHostInstancesForElementID = wrapped;
    }
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
  try {
    followCurrentAgent();
  } catch (e) {}
  withPublicInstances(hook.rendererInterfaces);
  if (!hook.__signalboxTapFix) {
    hook.__signalboxTapFix = true;
    hook.on('renderer', (e) => patch(e && e.renderer));
  }
  return 'ok';
})()

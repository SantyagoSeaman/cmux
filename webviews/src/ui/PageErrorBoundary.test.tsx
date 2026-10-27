// p10-stability-194: a render error in an app page shows the sad tab, not a blank page. The first
// crash reports itself and reloads the page once (a hidden page waits until it is shown); a crash
// again within REPEAT_WINDOW_MS shows the card with Reload. Every page root wraps the boundary.
import { afterEach, expect, test } from "bun:test";
import { readFileSync, readdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { JSDOM } from "jsdom";
import {
  PageErrorBoundary,
  REPEAT_WINDOW_MS,
  claimAutoReload,
  mayAutoReload,
  pageErrorText,
  type PageErrorHost,
} from "./PageErrorBoundary";

let dom: JSDOM | undefined;
let root: Root | undefined;
const scope = globalThis as Record<string, unknown>;
const saved = new Map<string, unknown>();

function installDom(): HTMLElement {
  dom = new JSDOM("<!doctype html><html><body><main id=root></main></body></html>", { url: "http://localhost/" });
  for (const key of ["window", "document", "navigator", "Node", "HTMLElement", "Element", "IS_REACT_ACT_ENVIRONMENT"])
    saved.set(key, scope[key]);
  Object.assign(scope, {
    window: dom.window,
    document: dom.window.document,
    navigator: dom.window.navigator,
    Node: dom.window.Node,
    HTMLElement: dom.window.HTMLElement,
    Element: dom.window.Element,
    IS_REACT_ACT_ENVIRONMENT: true,
  });
  return dom.window.document.getElementById("root")!;
}

afterEach(() => {
  act(() => root?.unmount());
  root = undefined;
  dom?.window.close();
  dom = undefined;
  for (const [key, value] of saved) scope[key] = value;
  saved.clear();
});

class MemoryStorage {
  readonly items = new Map<string, string>();
  getItem(key: string): string | null {
    return this.items.get(key) ?? null;
  }
  setItem(key: string, value: string): void {
    this.items.set(key, value);
  }
}

type FakeHost = PageErrorHost & {
  reloads: number;
  reports: { page: string; error: unknown; reloading: boolean }[];
  clock: number;
  isHidden: boolean;
  show(): void;
  store: MemoryStorage | null;
};

function fakeHost(options: { languages?: string[]; store?: MemoryStorage | null } = {}): FakeHost {
  let pending: (() => void) | null = null;
  const host: FakeHost = {
    reloads: 0,
    reports: [],
    clock: 1_000_000,
    isHidden: false,
    store: options.store === undefined ? new MemoryStorage() : options.store,
    reload: () => void host.reloads++,
    now: () => host.clock,
    memory: () => host.store,
    hidden: () => host.isHidden,
    whenVisible: (run) => {
      pending = run;
      return () => {
        pending = null;
      };
    },
    report: (page, error, reloading) => void host.reports.push({ page, error, reloading }),
    languages: () => options.languages ?? ["en"],
    show: () => {
      host.isHidden = false;
      const run = pending;
      pending = null;
      run?.();
    },
  };
  return host;
}

function Throws(): never {
  throw new Error("render failed");
}

function mount(host: PageErrorHost, child = <Throws />): HTMLElement {
  const element = installDom();
  // React logs every caught error itself; the boundary's own report is what this test reads.
  root = createRoot(element, { onCaughtError: () => {} });
  act(() =>
    root!.render(
      <PageErrorBoundary page="stats" host={host}>
        {child}
      </PageErrorBoundary>,
    ),
  );
  return element;
}

test("a healthy page renders its children and never reports", () => {
  const host = fakeHost();
  const element = mount(host, <p>ok</p>);
  expect(element.textContent).toBe("ok");
  expect(host.reports).toEqual([]);
  expect(host.reloads).toBe(0);
});

test("the first crash reports and reloads the page once", () => {
  const host = fakeHost();
  const element = mount(host);
  expect(host.reports).toHaveLength(1);
  expect(host.reports[0]!.page).toBe("stats");
  expect(host.reports[0]!.reloading).toBe(true);
  expect((host.reports[0]!.error as Error).message).toBe("render failed");
  expect(host.reloads).toBe(1);
  expect(element.querySelector('[role="alert"]')).not.toBeNull();
});

test("a crash again within the window shows the card, and Reload reloads", () => {
  const store = new MemoryStorage();
  const host = fakeHost({ store });
  expect(claimAutoReload(store, "stats", host.clock - REPEAT_WINDOW_MS / 2)).toBe(true);
  const element = mount(host);
  expect(host.reloads).toBe(0);
  expect(host.reports.map((report) => report.reloading)).toEqual([false]);
  const card = element.querySelector('[role="alert"]')!;
  expect(card.querySelector("h1")!.textContent).toBe("This page crashed");
  expect(card.querySelector("p")!.textContent).toBe("Other tabs are not affected. Reload to open the page again.");
  const button = card.querySelector("button")!;
  expect(button.textContent).toBe("Reload");
  act(() => button.click());
  expect(host.reloads).toBe(1);
});

test("a hidden page reloads only when it is shown", () => {
  const host = fakeHost();
  host.isHidden = true;
  mount(host);
  expect(host.reports.map((report) => report.reloading)).toEqual([true]);
  expect(host.reloads).toBe(0);
  host.clock += 5 * 60_000;
  host.show();
  expect(host.reloads).toBe(1);
  // The record is the reload's time, not the crash's.
  expect(mayAutoReload(host.store, "stats", host.clock + 1)).toBe(false);
});

test("unmounting a hidden crashed page cancels its pending reload", () => {
  const host = fakeHost();
  host.isHidden = true;
  mount(host);
  act(() => root!.unmount());
  root = undefined;
  host.show();
  expect(host.reloads).toBe(0);
});

test("a page with no session memory never reloads by itself", () => {
  const host = fakeHost({ store: null });
  const element = mount(host);
  expect(host.reloads).toBe(0);
  expect(host.reports.map((report) => report.reloading)).toEqual([false]);
  expect(element.querySelector("button")).not.toBeNull();
});

test("the reload record allows one automatic reload per window, per page", () => {
  const store = new MemoryStorage();
  expect(mayAutoReload(store, "files", 10)).toBe(true);
  expect(claimAutoReload(store, "files", 10)).toBe(true);
  expect(claimAutoReload(store, "files", 10 + REPEAT_WINDOW_MS - 1)).toBe(false);
  expect(claimAutoReload(store, "stats", 10)).toBe(true);
  expect(claimAutoReload(store, "files", 10 + REPEAT_WINDOW_MS)).toBe(true);
  const refusing = { getItem: () => null, setItem: () => {} };
  expect(claimAutoReload(refusing, "files", 10)).toBe(false);
  const throwing = {
    getItem: () => {
      throw new Error("SecurityError");
    },
    setItem: () => {},
  };
  expect(mayAutoReload(throwing, "files", 10)).toBe(false);
});

test("the card speaks the page's language, in its direction", () => {
  const german = pageErrorText(["de-DE"]);
  const english = pageErrorText(["en"]);
  expect(german.dir).toBe("ltr");
  expect(german.t("browser.pageGone.reload")).not.toBe("browser.pageGone.reload");
  expect(german.t("browser.pageGone.title.crashed")).not.toBe(english.t("browser.pageGone.title.crashed"));
  expect(pageErrorText(["ar"]).dir).toBe("rtl");
  const element = mount(fakeHost({ languages: ["ar"], store: null }));
  expect(element.querySelector('[role="alert"]')!.getAttribute("dir")).toBe("rtl");
});

test("every page root under pages/ renders inside PageErrorBoundary", () => {
  const pages = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../pages");
  const unwrapped: string[] = [];
  const walk = (dir: string) => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const file = path.join(dir, entry.name);
      if (entry.isDirectory()) {
        if (entry.name !== "generated" && entry.name !== "node_modules") walk(file);
        continue;
      }
      // settings/testing.tsx mounts sections for tests; it is not a page root.
      if (!/\.tsx?$/.test(entry.name) || /\.test\.tsx?$/.test(entry.name) || entry.name === "testing.tsx") continue;
      const text = readFileSync(file, "utf8");
      const roots = text.match(/\b(?:createRoot|makeRoot)\(/g)?.length ?? 0;
      const wrapped = text.match(/<PageErrorBoundary\b/g)?.length ?? 0;
      if (roots > wrapped) unwrapped.push(path.relative(pages, file));
    }
  };
  walk(pages);
  expect(unwrapped).toEqual([]);
});

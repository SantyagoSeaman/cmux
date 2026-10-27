// Every app page's last error boundary: the page's sad tab. Without it, a render error unmounts the
// whole React root and leaves a blank page. The first crash reports itself (console.error, which
// ConsoleErrorCapture sends to the error journal) and reloads the page once; a crash again within
// REPEAT_WINDOW_MS of that reload shows the card, whose Reload button the person presses.
// Chrome does the same: chrome/browser/ui/sad_tab.cc shows the card for a crashed renderer, and a
// crashed page that is hidden reloads only when it is shown again
// (content/browser/web_contents/web_contents_impl.cc, kReloadHiddenTabsWithCrashedSubframes), so a
// hidden page here waits for visibilitychange before its automatic reload.
import { Component, type ReactNode } from "react";
import { isRtlLanguage, resolveLanguage, type StringTable } from "../pages/shared/i18n";
import table from "./page-error/generated/strings.json";
import "./pageErrorBoundary.css";

/** A crash within this long after the last automatic reload shows the card instead of reloading again. */
export const REPEAT_WINDOW_MS = 60_000;

/** The page's environment, injected so tests drive it without a real document. */
export interface PageErrorHost {
  reload(): void;
  now(): number;
  /** The document's session memory (sessionStorage), which a reload keeps; null when there is none. */
  memory(): Pick<Storage, "getItem" | "setItem"> | null;
  hidden(): boolean;
  /** Runs `run` once when the page is next shown; returns a cancel. */
  whenVisible(run: () => void): () => void;
  /** Reports the crash; `reloading` when the page reloads by itself. */
  report(page: string, error: unknown, reloading: boolean): void;
  /** The languages to pick the card's strings for, most preferred first. */
  languages(): readonly string[];
}

/** The live document's host. */
export function browserPageErrorHost(win: Window = window): PageErrorHost {
  const doc = win.document;
  return {
    reload: () => win.location.reload(),
    now: () => Date.now(),
    memory: () => {
      try {
        return win.sessionStorage;
      } catch {
        return null;
      }
    },
    hidden: () => doc.visibilityState === "hidden",
    whenVisible: (run) => {
      const onChange = () => {
        if (doc.visibilityState === "hidden") return;
        doc.removeEventListener("visibilitychange", onChange);
        run();
      };
      doc.addEventListener("visibilitychange", onChange);
      return () => doc.removeEventListener("visibilitychange", onChange);
    },
    report: (page, error, reloading) =>
      console.error(`page ${page} crashed; ${reloading ? "reloading it once" : "showing its reload card"}:`, error),
    languages: () => [doc.documentElement.lang, ...(win.navigator.languages ?? [])].filter(Boolean),
  };
}

const memoryKey = (page: string) => `cmux.pageError.autoReloadAt.${page}`;

/** Whether `page` may reload by itself now: no automatic reload within REPEAT_WINDOW_MS. */
export function mayAutoReload(memory: Pick<Storage, "getItem"> | null, page: string, now: number): boolean {
  if (!memory) return false;
  try {
    const last = Number(memory.getItem(memoryKey(page)) ?? "");
    return !(last > 0 && now - last < REPEAT_WINDOW_MS);
  } catch {
    return false;
  }
}

/**
 * Records an automatic reload of `page` at `now` and says whether it may happen: false when one
 * happened within REPEAT_WINDOW_MS, or when the memory cannot keep the record (a reload it could not
 * remember could repeat forever).
 */
export function claimAutoReload(
  memory: Pick<Storage, "getItem" | "setItem"> | null,
  page: string,
  now: number,
): boolean {
  if (!memory || !mayAutoReload(memory, page, now)) return false;
  try {
    memory.setItem(memoryKey(page), String(now));
    return memory.getItem(memoryKey(page)) === String(now);
  } catch {
    return false;
  }
}

type Props = {
  /** The page's name (its folder under pages/): the report's and the reload record's key. */
  page: string;
  children?: ReactNode;
  host?: PageErrorHost;
};
type State = { failed: boolean };

export class PageErrorBoundary extends Component<Props, State> {
  override state: State = { failed: false };
  private cancelReload: (() => void) | null = null;

  static getDerivedStateFromError(): State {
    return { failed: true };
  }

  private host(): PageErrorHost {
    return this.props.host ?? browserPageErrorHost();
  }

  override componentDidCatch(error: unknown): void {
    const host = this.host();
    const { page } = this.props;
    const reloading = this.cancelReload === null && mayAutoReload(host.memory(), page, host.now());
    host.report(page, error, reloading);
    if (!reloading) return;
    const autoReload = () => {
      this.cancelReload = null;
      if (claimAutoReload(host.memory(), page, host.now())) host.reload();
    };
    if (host.hidden()) this.cancelReload = host.whenVisible(autoReload);
    else autoReload();
  }

  override componentWillUnmount(): void {
    this.cancelReload?.();
    this.cancelReload = null;
  }

  override render(): ReactNode {
    if (!this.state.failed) return this.props.children;
    const host = this.host();
    return <PageCrashed text={pageErrorText(host.languages())} onReload={() => host.reload()} />;
  }
}

/** The card's text in the first of `languages` the table has (else English), and its direction. */
export function pageErrorText(languages: readonly string[]): PageErrorText {
  const strings = table as StringTable;
  const language = resolveLanguage(languages, Object.keys(strings));
  const local = strings[language] ?? {};
  const english = strings.en ?? {};
  return {
    dir: isRtlLanguage(language) ? "rtl" : "ltr",
    t: (key) => local[key] ?? english[key] ?? key,
  };
}

type PageErrorKey = "browser.pageGone.title.crashed" | "browser.pageGone.message" | "browser.pageGone.reload";
export type PageErrorText = { dir: "ltr" | "rtl"; t: (key: PageErrorKey) => string };

/** The sad tab: what happened, that the rest of the app is fine, and Reload. */
export function PageCrashed({ text: { dir, t }, onReload }: { text: PageErrorText; onReload: () => void }) {
  return (
    <div className="page-error" role="alert" dir={dir} data-page-error="crashed">
      <div className="page-error-card">
        <h1 className="page-error-title">{t("browser.pageGone.title.crashed")}</h1>
        <p className="page-error-message">{t("browser.pageGone.message")}</p>
        <button type="button" className="page-error-reload" onClick={onReload}>
          {t("browser.pageGone.reload")}
        </button>
      </div>
    </div>
  );
}

// l10n-allow-file: gallery fixtures, not shipped UI.
// The sad tab of every app page: a page whose render throws. The fixture host never reloads the
// gallery; the card's Reload button counts its presses instead.
import { useState } from "react";
import { componentEntry } from "../gallery/format";
import { PageErrorBoundary, browserPageErrorHost, type PageErrorHost } from "./PageErrorBoundary";

type Props = { crash: boolean; languages?: string[] };

function ThrowingPage(): never {
  throw new Error("gallery fixture: the page's render threw");
}

/** A crash that has already reloaded once: the card stays, as on a repeat crash. */
function fixtureHost(languages: readonly string[] | undefined, onReload: () => void): PageErrorHost {
  const live = browserPageErrorHost();
  return {
    ...live,
    reload: onReload,
    memory: () => null,
    report: () => {},
    languages: () => languages ?? live.languages(),
  };
}

function PageErrorGallery({ crash, languages }: Props) {
  const [reloads, setReloads] = useState(0);
  const [host] = useState(() => fixtureHost(languages, () => setReloads((count) => count + 1)));
  return (
    <div style={{ height: "100%", display: "grid", gridTemplateRows: "1fr auto" }}>
      <PageErrorBoundary key={reloads} page="gallery" host={host}>
        {crash ? <ThrowingPage /> : <p style={{ padding: 16 }}>The page drew without an error.</p>}
      </PageErrorBoundary>
      <p style={{ padding: 8, fontSize: "var(--text-caption)", opacity: 0.6 }} data-gallery-reloads={reloads}>
        Reload pressed {reloads} times
      </p>
    </div>
  );
}

export default componentEntry<Props>({
  id: "ui.page-error-boundary",
  title: "Page crash card (sad tab)",
  area: "Pages",
  covers: ["ui/PageErrorBoundary.tsx"],
  keywords: ["sad tab", "error boundary", "crash", "reload"],
  height: 320,
  load: async () => PageErrorGallery,
  variants: {
    crashed: { note: "A page whose render throws, after its one automatic reload.", props: { crash: true } },
    "crashed-german": {
      note: "The same card in German: its strings are the browser sad tab's.",
      props: { crash: true, languages: ["de"] },
    },
    "crashed-arabic": {
      note: "Right-to-left: the card follows the page's language direction.",
      props: { crash: true, languages: ["ar"] },
    },
    healthy: { note: "A page that renders: the boundary draws nothing of its own.", props: { crash: false } },
  },
});

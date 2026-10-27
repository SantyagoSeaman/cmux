// Settings > Menus: reordering by pointer, at any depth. A row is dragged by its grip with pointer
// events and pointer capture, never HTML5 drag and drop: a native drag runs in AppKit's drag loop,
// so the web view sees no mouse-up and the drop's save is not the person's gesture (the host
// refuses it), and per-row dragenter/dragleave made the drop line blink. The drop place comes from
// the pointer's height over the rows as they stood when the drag began (rows slide while it runs,
// and measuring them then would move the target under a still pointer): before a row, after it,
// or into a submenu (the middle of a submenu's row). The rows between the dragged block (a row and
// what an open submenu shows under it) and the drop slide apart to open the gap, the dragged row
// moves sideways to the depth it will land at, and the save runs on the pointer-up the web view
// does see.
import { samePath, within, type Path } from "./menuLayouts";

/** One drawn row as the drag measured it: its place in the drawn list and in the tree. */
export type RowBox = {
  flat: number;
  path: Path;
  depth: number;
  top: number;
  bottom: number;
  /** A submenu: rows drop into it. */
  folder: boolean;
  /** An open submenu: its rows are drawn under it. */
  open: boolean;
  /** How many rows a submenu holds. */
  children: number;
};

/**
 * Where a drop lands: index `index` of the submenu at `parent` (`[]`: the top level), as counted
 * before the move. `flat` is where the gap opens in the drawn list (null for a closed submenu,
 * which takes the row out of sight); `into` names the submenu row to highlight.
 */
export type DropTarget = { parent: Path; index: number; depth: number; flat: number | null; into?: Path };

/** Pointer travel before a press on a grip becomes a drag (a plain click on it does nothing). */
export const DRAG_THRESHOLD = 4;

/** How far one level of submenu indents a row, in CSS pixels. */
export const INDENT = 18;

/** The drawn rows the node at `path` covers: itself and what an open submenu shows under it. */
export function block(rows: RowBox[], path: Path): { first: number; last: number } {
  const first = rows.findIndex((row) => samePath(row.path, path));
  let last = first;
  while (last + 1 < rows.length && within(rows[last + 1]!.path, path)) last += 1;
  return { first, last };
}

/**
 * The drop target at pointer height `y` for the node at `dragged`, or null where it would not
 * move. A submenu's row splits in four: its top quarter drops before it, its middle into it (at
 * the end), its bottom quarter into an open one at its start or after a closed one; any other row's
 * upper half drops before it and its lower half after it. Below the last row is the top level's end.
 */
export function dropTarget(rows: RowBox[], y: number, dragged: Path, topCount: number): DropTarget | null {
  const own = block(rows, dragged);
  const parentOf = (path: Path) => path.slice(0, -1);
  const last = (path: Path) => path[path.length - 1]!;
  let target: DropTarget | null = null;
  for (const row of rows) {
    if (y >= row.bottom) continue;
    if (row.flat >= own.first && row.flat <= own.last) return null;
    const height = row.bottom - row.top;
    const before: DropTarget = { parent: parentOf(row.path), index: last(row.path), depth: row.depth, flat: row.flat };
    if (row.folder) {
      if (y < row.top + height / 4) target = before;
      else if (y <= row.bottom - height / 4) {
        const end = block(rows, row.path).last + 1;
        target = { parent: row.path, index: row.children, depth: row.depth + 1, flat: row.open ? end : null, into: row.path };
      } else if (row.open) target = { parent: row.path, index: 0, depth: row.depth + 1, flat: row.flat + 1 };
      else target = { parent: parentOf(row.path), index: last(row.path) + 1, depth: row.depth, flat: row.flat + 1 };
    } else {
      target =
        y < row.top + height / 2
          ? before
          : { parent: parentOf(row.path), index: last(row.path) + 1, depth: row.depth, flat: row.flat + 1 };
    }
    break;
  }
  target ??= { parent: [], index: topCount, depth: 0, flat: rows.length };
  // Back where it was: no move.
  if (samePath(target.parent, parentOf(dragged)) && (target.index === last(dragged) || target.index === last(dragged) + 1)) return null;
  return target;
}

/**
 * How far drawn row `flat` slides while the block `[first, last]` (height `height`) is dragged to
 * a gap at drawn index `gap`: the rows between them slide one block toward its old place.
 */
export function shift(flat: number, first: number, last: number, gap: number | null, height: number): number {
  if (gap === null || (flat >= first && flat <= last)) return 0;
  if (gap > last && flat > last && flat < gap) return -height;
  if (gap <= first && flat >= gap && flat < first) return height;
  return 0;
}

/** The drawn rows of `list` (each carries `data-flat`, `data-path` and the rest), in page coordinates. */
export function rowBoxes(list: HTMLElement): RowBox[] {
  return [...list.querySelectorAll<HTMLElement>(":scope > [data-flat]")].map((element) => {
    const rect = element.getBoundingClientRect();
    return {
      flat: Number(element.dataset.flat),
      path: (element.dataset.path ?? "").split(".").filter(Boolean).map(Number),
      depth: Number(element.dataset.depth ?? 0),
      top: rect.top,
      bottom: rect.bottom,
      folder: element.dataset.folder !== undefined,
      open: element.dataset.open !== undefined,
      children: Number(element.dataset.children ?? 0),
    };
  });
}

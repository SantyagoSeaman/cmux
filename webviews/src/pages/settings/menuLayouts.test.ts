// Settings > Menus' tree edits (menuLayouts.ts) and pointer geometry (menuDrag.ts): a node moves to
// any index of any submenu, counted before the move; a submenu never into itself; a drop lands
// before a row, after it, or in a submenu; the rows between a dragged block and its drop slide.
import { expect, test } from "bun:test";
import { dropTarget, shift, type RowBox } from "./menuDrag";
import { deleteGroup, destinations, move, step, type Folder } from "./menuLayouts";
import type { MenuLayout } from "./ops";

const layout: MenuLayout = {
  items: ["a", "b", { key: "folder:more", items: ["c", { key: "folder:options", items: ["d"] }] }, "e"],
  hidden: [],
};

test("a node moves to any index of any submenu, the old place still counted", () => {
  expect(move(layout, [0], [], 3).items).toEqual(["b", { key: "folder:more", items: ["c", { key: "folder:options", items: ["d"] }] }, "a", "e"]);
  expect(move(layout, [3], [2, 1], 0).items[2]).toEqual({ key: "folder:more", items: ["c", { key: "folder:options", items: ["e", "d"] }] });
  expect(move(layout, [2, 1, 0], [], 0).items[0]).toBe("d");
  // Into itself or inside itself: nothing changes.
  expect(move(layout, [2], [2, 1], 0)).toBe(layout);
  expect(step(layout, [1], -1).items.slice(0, 2)).toEqual(["b", "a"]);
  expect(step(layout, [0], -1)).toBe(layout);
});

test("Move to… lists every submenu but the node's own and those inside it", () => {
  const title = (node: Folder) => node.key;
  expect(destinations(layout, [0], title).map((d) => d.trail.join(" › "))).toEqual(["", "folder:more", "folder:more › folder:options"]);
  expect(destinations(layout, [2], title).map((d) => d.trail.join(" › "))).toEqual([""]);
});

test("deleting the person's submenu leaves its rows in its place", () => {
  const mine: MenuLayout = { items: ["a", { key: "group:x", title: "X", items: ["b", "c"] }, "d"], hidden: ["group:x"] };
  expect(deleteGroup(mine, [1])).toEqual({ items: ["a", "b", "c", "d"], hidden: [] });
});

const rows: RowBox[] = [
  { flat: 0, path: [0], depth: 0, top: 100, bottom: 124, folder: false, open: false, children: 0 },
  { flat: 1, path: [1], depth: 0, top: 124, bottom: 148, folder: true, open: true, children: 1 },
  { flat: 2, path: [1, 0], depth: 1, top: 148, bottom: 172, folder: false, open: false, children: 0 },
  { flat: 3, path: [2], depth: 0, top: 172, bottom: 196, folder: true, open: false, children: 4 },
];

test("a drop lands before a row, after it, or inside a submenu", () => {
  // A submenu row's top quarter: before it (the closed submenu below dragged above the open one).
  expect(dropTarget(rows, 125, [2], 3)).toEqual({ parent: [], index: 1, depth: 0, flat: 1 });
  // A submenu's middle: into it, at its end.
  expect(dropTarget(rows, 184, [0], 3)).toEqual({ parent: [2], index: 4, depth: 1, flat: null, into: [2] });
  // An open submenu's bottom quarter: at its start.
  expect(dropTarget(rows, 146, [0], 3)).toEqual({ parent: [1], index: 0, depth: 1, flat: 2 });
  // A row's lower half inside a submenu: after it, in that submenu.
  expect(dropTarget(rows, 168, [0], 3)).toEqual({ parent: [1], index: 1, depth: 1, flat: 3 });
  // Below every row: the end of the top level.
  expect(dropTarget(rows, 400, [0], 3)).toEqual({ parent: [], index: 3, depth: 0, flat: 4 });
  // Over itself, or back where it was: no move.
  expect(dropTarget(rows, 110, [0], 3)).toBeNull();
  // A submenu never drops inside itself.
  expect(dropTarget(rows, 160, [1], 3)).toBeNull();
});

test("the rows between a dragged block and its gap slide one block toward its old place", () => {
  expect([0, 1, 2, 3].map((flat) => shift(flat, 3, 3, 1, 24))).toEqual([0, 24, 24, 0]);
  expect([0, 1, 2, 3].map((flat) => shift(flat, 0, 0, 4, 24))).toEqual([0, -24, -24, -24]);
  // A two-row block (an open submenu) moves two rows' worth.
  expect([0, 1, 2, 3].map((flat) => shift(flat, 1, 2, 0, 48))).toEqual([48, 0, 0, 0]);
  expect(shift(1, 3, 3, null, 24)).toBe(0);
});

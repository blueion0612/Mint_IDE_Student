"""Draw the README hero: the exam lifecycle, from writing to a sealed submission.

    python docs/figures/make_hero.py

Writes hero_flow.png and hero_flow-dark.png.
"""
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__)) + os.sep
sys.path.insert(0, HERE)

import figstyle  # noqa: E402
import matplotlib.pyplot as plt  # noqa: E402
from matplotlib.patches import FancyArrowPatch, FancyBboxPatch  # noqa: E402

RECORDED = [
    "every insert and delete, classified as typing, paste or undo",
    "clipboard contents and where they came from",
    "window focus, so leaving the IDE is visible",
    "file integrity, flagging edits made outside the editor",
    "the screen, recorded for the length of the session",
]


def flow(T):
    fig, ax = plt.subplots(figsize=(figstyle.WIDTH, 4.2))
    ax.set_xlim(0, 94)
    ax.set_ylim(0, 42)
    ax.axis("off")
    G, GF, D, DF = T["green"], T["green_fill"], T["gold"], T["gold_fill"]

    def box(x, y, w, h, title, sub, edge=None, face=None, tcol=None):
        ax.add_patch(FancyBboxPatch((x, y), w, h, boxstyle="round,pad=0,rounding_size=1.4",
                                    linewidth=1.4, edgecolor=edge or T["line"],
                                    facecolor=face or T["fill"], zorder=2))
        ax.text(x + w / 2, y + h / 2 + 1.9, title, ha="center", va="center",
                fontsize=figstyle.TITLE, color=tcol or T["ink"], fontweight="bold", zorder=3)
        ax.text(x + w / 2, y + h / 2 - 2.3, sub, ha="center", va="center",
                fontsize=figstyle.SMALL, color=T["muted"], zorder=3)

    def arrow(x0, y0, x1, y1, c=None):
        ax.add_patch(FancyArrowPatch((x0, y0), (x1, y1), arrowstyle="-|>", mutation_scale=12,
                                     linewidth=1.5, color=c or T["line"], shrinkA=0, shrinkB=0, zorder=1))

    W, H, Y = 26.0, 11.0, 27.0
    box(2.0, Y, W, H, "Student writes", "editor, files, run")
    arrow(28.0, Y + H / 2, 31.5, Y + H / 2)
    box(31.5, Y, W, H, "Session recorded", "five streams, below", D, DF)
    arrow(57.5, Y + H / 2, 61.0, Y + H / 2, G)
    box(61.0, Y, 31.0, H, "Sealed submission", "AES-256, keyed to a hashed ID", G, GF)

    ax.text(44.5, 21.5, "what the session records", ha="center", fontsize=figstyle.BODY,
            color=D, fontweight="bold")
    for i, line in enumerate(RECORDED):
        ax.text(47.0, 17.4 - i * 3.5, line, ha="center", va="center",
                fontsize=figstyle.SMALL, color=T["muted"])
    ax.text(76.5, Y - 3.6, "opened only by MINT Grader", ha="center",
            fontsize=figstyle.SMALL, color=G, fontweight="bold")
    return fig


if __name__ == "__main__":
    figstyle.save_both(flow, HERE + "hero_flow")

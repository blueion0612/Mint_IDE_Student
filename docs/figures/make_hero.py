"""Render the exam lifecycle figure used at the top of the README.

    python docs/figures/make_hero.py

Writes hero_flow.png and hero_flow-dark.png.
"""
import os

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.patches import FancyBboxPatch, FancyArrowPatch

THEMES = {
    "light": dict(bg="white", ink="#1c2530", muted="#5b6875", line="#b9c3cf",
                  fill="#eef2f6", watch="#c8683f", seal="#4a7fb5", out="#3f7d5a",
                  fwatch="#fbeee7", fseal="#eaf1f8", fout="#e9f2ec"),
    "dark": dict(bg="#0d1117", ink="#e6edf3", muted="#9198a1", line="#3d444d",
                 fill="#161b22", watch="#e08a5c", seal="#6ea8dd", out="#5aa87a",
                 fwatch="#2a1c14", fseal="#12202f", fout="#12241a"),
}

RECORDED = [
    "every insert and delete, classified as typing, paste or undo",
    "clipboard contents and where they came from",
    "window focus, so leaving the IDE is visible",
    "file integrity, flagging edits made outside the editor",
    "the screen, recorded for the length of the session",
]

HERE = os.path.dirname(os.path.abspath(__file__))


def render(theme, out):
    T = THEMES[theme]
    fig, ax = plt.subplots(figsize=(9.4, 4.2), dpi=170)
    ax.set_xlim(0, 94)
    ax.set_ylim(0, 42)
    ax.axis("off")
    fig.patch.set_facecolor(T["bg"])

    def box(x, y, w, h, title, sub, edge=None, face=None, tcol=None):
        ax.add_patch(FancyBboxPatch((x, y), w, h, boxstyle="round,pad=0,rounding_size=1.4",
                                    linewidth=1.4, edgecolor=edge or T["line"],
                                    facecolor=face or T["fill"], zorder=2))
        ax.text(x + w / 2, y + h / 2 + (1.9 if sub else 0), title, ha="center", va="center",
                fontsize=11.4, color=tcol or T["ink"], fontweight="bold", zorder=3)
        if sub:
            ax.text(x + w / 2, y + h / 2 - 2.3, sub, ha="center", va="center",
                    fontsize=9.0, color=T["muted"], zorder=3)

    def arrow(x0, y0, x1, y1, c=None):
        ax.add_patch(FancyArrowPatch((x0, y0), (x1, y1), arrowstyle="-|>", mutation_scale=12,
                                     linewidth=1.5, color=c or T["line"], shrinkA=0, shrinkB=0, zorder=1))

    W, H, Y = 26.0, 11.0, 27.0
    box(2.0, Y, W, H, "Student writes", "editor, files, run")
    arrow(28.0, Y + H / 2, 31.5, Y + H / 2)
    box(31.5, Y, W, H, "Session recorded", "five streams, below", T["watch"], T["fwatch"])
    arrow(57.5, Y + H / 2, 61.0, Y + H / 2, T["seal"])
    box(61.0, Y, 31.0, H, "Sealed submission", "AES-256, keyed to a hashed ID", T["seal"], T["fseal"])

    ax.text(44.5, 21.5, "what the session records", ha="center", fontsize=9.6,
            color=T["watch"], fontweight="bold")
    for i, line in enumerate(RECORDED):
        ax.text(47.0, 17.4 - i * 3.5, line, ha="center", va="center",
                fontsize=8.8, color=T["muted"])

    ax.text(76.5, Y - 3.6, "opened only by MINT Grader", ha="center",
            fontsize=8.8, color=T["out"], fontweight="bold")

    fig.tight_layout(pad=0.2)
    fig.savefig(out, dpi=170, bbox_inches="tight", facecolor=T["bg"])
    plt.close(fig)
    print("wrote", out)


if __name__ == "__main__":
    render("light", os.path.join(HERE, "hero_flow.png"))
    render("dark", os.path.join(HERE, "hero_flow-dark.png"))

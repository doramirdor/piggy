// Per-saver line icon in a colored rounded-square tile (replaces the emoji
// glyphs). SF-Symbols-adjacent 1.8px line icons, keyed by saver id.

interface IconDef {
  fg: string;
  bg: string;
  node: JSX.Element;
}

const broom = (
  <>
    <path d="M17.5 5.5 11 12" />
    <path d="M11 12 8.4 14.6 11.4 17.6 14 15 Z" />
    <path d="M9.1 16.2 7.9 18.4M11.2 17.6 10.6 20M13 16.8 13.2 19.2" />
  </>
);
const monitor = (
  <>
    <rect x="3" y="5" width="18" height="13" rx="2" />
    <path d="M3 8.5h18" />
    <circle cx="5.6" cy="6.7" r="0.5" fill="currentColor" stroke="none" />
    <circle cx="7.6" cy="6.7" r="0.5" fill="currentColor" stroke="none" />
    <path d="M6 14 8 12 10 15 12 11 14 14.5 16 12.5 18 14" />
  </>
);
const terseLines = (
  <>
    <path d="M5 7h14" />
    <path d="M5 12h10" />
    <path d="M5 17h6" />
  </>
);
const codeFile = (
  <>
    <path d="M14 3H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9z" />
    <path d="M14 3v6h6" />
    <path d="M10 12.5 8.5 14 10 15.5M14 12.5 15.5 14 14 15.5" />
  </>
);
const pig = (
  <>
    <ellipse cx="12" cy="13.5" rx="7" ry="5.5" />
    <circle cx="16" cy="14" r="1.6" />
    <circle cx="15.2" cy="11.5" r="0.5" fill="currentColor" stroke="none" />
  </>
);

const ICONS: Record<string, IconDef> = {
  sweep: { fg: "#22c55e", bg: "rgba(34,197,94,0.14)", node: broom },
  rtk: { fg: "#3b82f6", bg: "rgba(59,130,246,0.14)", node: monitor },
  caveman: { fg: "#f59e0b", bg: "rgba(245,158,11,0.15)", node: terseLines },
  ponytail: { fg: "#8b5cf6", bg: "rgba(139,92,246,0.15)", node: codeFile },
};

const FALLBACK: IconDef = { fg: "var(--text-2)", bg: "rgba(127,127,127,0.14)", node: pig };

export function SaverIcon({ id, size = 38 }: { id: string; size?: number }) {
  const def = ICONS[id] ?? FALLBACK;
  return (
    <span
      className="sicon"
      style={{ width: size, height: size, background: def.bg, color: def.fg }}
    >
      <svg
        width={size * 0.55}
        height={size * 0.55}
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.8"
        strokeLinecap="round"
        strokeLinejoin="round"
        aria-hidden
      >
        {def.node}
      </svg>
    </span>
  );
}

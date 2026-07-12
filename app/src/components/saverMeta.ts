// Presentation-only metadata for saver rows: the emoji glyph + tint used for the
// little rounded icon, keyed by saver id (mirrors the mockup's icon choices).

interface IconMeta {
  glyph: string;
  tint: string;
}

const ICONS: Record<string, IconMeta> = {
  rtk: { glyph: "🤫", tint: "rgba(10,132,255,0.16)" },
  caveman: { glyph: "✂️", tint: "rgba(255,159,10,0.16)" },
  ponytail: { glyph: "🧶", tint: "rgba(191,90,242,0.16)" },
  sweep: { glyph: "🧹", tint: "rgba(48,209,88,0.16)" },
};

export function saverIcon(id: string): IconMeta {
  return ICONS[id] ?? { glyph: "🐷", tint: "rgba(127,127,127,0.16)" };
}

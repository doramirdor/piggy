import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { IS_MOCK } from "./ipc";
// No bundled webfont. v2 sets type entirely in faces macOS already ships
// (Hoefler Text, SF, SF Mono), which is the only platform Piggy runs on: no
// FOUT, one fewer dependency, and 218KB of woff2 out of the binary.
import "./index.css";

// The dev mock adds a desktop-like backdrop so the panel reads well in a plain
// browser tab; the real Tauri window keeps <body> transparent for vibrancy.
if (IS_MOCK) {
  document.body.classList.add("mock");
}

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);

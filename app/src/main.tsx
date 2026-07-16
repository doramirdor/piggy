import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { IS_MOCK } from "./ipc";
// Inter - bundled locally (woff2 served from 'self', CSP-safe & offline). The
// variable file covers every weight the brand system uses (Regular→Bold).
import "@fontsource-variable/inter";
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

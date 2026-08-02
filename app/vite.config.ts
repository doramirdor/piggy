import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// Piggy's Vite + Vitest config. The Tauri panel is a fixed 360×600 WKWebView, so
// the build targets Safari 15 (the macOS WebKit baseline).
export default defineConfig({
  plugins: [react(), tailwindcss()],
  clearScreen: false,
  server: {
    // Bind IPv4 explicitly. Node 17+ resolves "localhost" to ::1 first, so the
    // default host leaves Vite listening on [::1] only - and the Tauri WKWebView
    // resolves localhost to 127.0.0.1, fails to connect, and shows a blank white
    // window. Browsers hide this by falling back to IPv6; the webview does not.
    host: "127.0.0.1",
    port: 5173,
    strictPort: true,
  },
  build: {
    outDir: "dist",
    target: "safari15",
    sourcemap: false,
    emptyOutDir: true,
  },
  test: {
    environment: "node",
    include: ["src/**/*.test.ts"],
  },
});

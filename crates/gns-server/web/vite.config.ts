import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// `npm run dev` proxies the API to a running gns-server (default port 8787).
const target = process.env.GNS_SERVER_URL ?? "http://127.0.0.1:8787";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  server: { proxy: { "/api": { target, changeOrigin: true } } },
  build: { outDir: "dist", emptyOutDir: true, chunkSizeWarningLimit: 1000 },
});

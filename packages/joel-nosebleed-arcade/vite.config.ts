import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

const backendPort = Number.parseInt(process.env.ARCADE_PORT ?? "4300", 10) || 4300;

export default defineConfig({
  plugins: [react()],
  build: {
    outDir: "dist/client",
    emptyOutDir: true
  },
  server: {
    host: "127.0.0.1",
    port: 5174,
    strictPort: true,
    proxy: {
      "/api": {
        target: `http://127.0.0.1:${backendPort}`,
        changeOrigin: false
      },
      "/ws": {
        target: `ws://127.0.0.1:${backendPort}`,
        ws: true,
        changeOrigin: false
      }
    }
  }
});

import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Relative base so the built assets work from the embedded server at any path.
export default defineConfig({
  plugins: [react()],
  base: "./",
  build: { outDir: "dist", emptyOutDir: true, target: "es2020" },
  server: {
    port: 5273,
    // `cargo run -- ui --dev` runs the API separately; proxy to it during frontend work.
    proxy: { "/api": "http://127.0.0.1:7391" },
  },
});

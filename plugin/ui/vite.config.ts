import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// Relative base: the console serves the page under /plugin-ui/plugover/.
export default defineConfig({
	base: "./",
	plugins: [react(), tailwindcss()],
	build: { outDir: "../dist/ui", emptyOutDir: true },
	server: { proxy: { "/api": "http://127.0.0.1:7276" } },
});

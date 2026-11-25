import { defineConfig } from 'vite';

// Axum serves the shell and these two assets directly from the binary.
export default defineConfig({
  build: {
    target: 'es2022',
    modulePreload: false,
    cssCodeSplit: false,
    rollupOptions: {
      output: {
        inlineDynamicImports: true,
        entryFileNames: 'app.js',
        assetFileNames: '[name][extname]',
      },
    },
  },
});

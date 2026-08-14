// Ambient module declarations for non-TS imports the SPA pulls in.
// Vite handles `.css` imports as side-effect modules at build time; the
// TS compiler needs a hint so `import "./lib/tokens.css"` typechecks.

declare module "*.css";

// Vite's `?raw` query — a module's source text as a default-exported
// string. Used by the source-level pins (e.g.
// `statistics-integrity-banner.test.ts`), which assert on a component's
// markup in a package that mounts no components.
declare module "*?raw" {
  const content: string;
  export default content;
}

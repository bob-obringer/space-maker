// Entry point. Outside of Tauri (plain browser) fall back to a mock backend so
// the UI can be previewed with `bun run dev`.
if (!("__TAURI_INTERNALS__" in window)) {
  await import("./mock");
}
await import("./main");

export {};

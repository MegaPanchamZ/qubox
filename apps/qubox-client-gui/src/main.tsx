import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import "./App.css";

const rootElement = document.getElementById("root");
if (!rootElement) {
  throw new Error("missing #root element");
}

const root = createRoot(rootElement);

function mount(key = 0) {
  root.render(
    <StrictMode>
      <App key={key} />
    </StrictMode>,
  );
}

mount();

// Browser-mode e2e: remount after registering tauri mocks (full reload wipes mocks).
declare global {
  interface Window {
    __QUBOX_E2E_REMOUNT__?: () => void;
  }
}
window.__QUBOX_E2E_REMOUNT__ = () => mount(Date.now());

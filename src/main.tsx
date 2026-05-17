import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { TrayApp } from "./TrayApp";
import "./index.css";

// task22 / AC-8.1: the same bundle drives both the main window and
// the tray window. Tauri opens the tray window with
// `url: "index.html#/tray"`, so we route on `location.hash`.
const isTray =
  typeof window !== "undefined" && window.location.hash === "#/tray";

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>{isTray ? <TrayApp /> : <App />}</React.StrictMode>,
);

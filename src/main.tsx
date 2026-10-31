import React from "react";
import ReactDOM from "react-dom/client";
import App from "./ui/App";
import Popup from "./ui/Popup";
import Reader from "./ui/Reader";
import { getCurrentWindow } from "@tauri-apps/api/window";
import "./ui/styles.css";

const label = getCurrentWindow().label;
const Root = label === "reader" ? Reader : label === "popup-menu" ? Popup : App;

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <Root />
  </React.StrictMode>,
);

import { createApp } from "vue";
import { createPinia } from "pinia";
import App from "./App.vue";
import { installFrontendLogCapture } from "./frontendLog";
import { i18n } from "./i18n";
import { router } from "./router";
import "./style.css";

// Capture the webview console into the backend's rolling log file BEFORE the app
// mounts, so an error thrown during store setup or the first render is recorded
// too. No-op outside Tauri (a plain browser / vitest keeps its console intact).
installFrontendLogCapture();

createApp(App).use(createPinia()).use(i18n).use(router).mount("#app");

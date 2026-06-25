import { createI18n } from "vue-i18n";
import enUS from "./locales/en-US.json";

const messages = { "en-US": enUS };

const detected =
  typeof navigator !== "undefined" && navigator.language ? navigator.language : "en-US";

export const i18n = createI18n({
  legacy: false,
  globalInjection: true,
  locale: messages[detected as keyof typeof messages] ? detected : "en-US",
  fallbackLocale: "en-US",
  messages,
});

export type MessageSchema = typeof enUS;

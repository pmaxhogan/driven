import vue from "eslint-plugin-vue";
import vueParser from "vue-eslint-parser";
import tseslint from "typescript-eslint";
import vueI18n from "@intlify/eslint-plugin-vue-i18n";
// Prettier owns formatting (M9 tail): this config turns off every ESLint rule
// that conflicts with Prettier (incl. eslint-plugin-vue's template-formatting
// rules like vue/max-attributes-per-line and vue/html-self-closing), so
// `prettier --write` and `eslint` never fight over the same code. It MUST be
// the LAST entry so its disables win.
import prettierConfig from "eslint-config-prettier";

export default [
  {
    ignores: [
      "dist/**",
      "coverage/**",
      "node_modules/**",
      "src-tauri/**",
      "*.config.js",
      "*.config.ts",
      "pnpm-lock.yaml",
      "tsconfig.json",
      "**/*.lock",
    ],
  },
  ...tseslint.configs.recommended,
  ...vue.configs["flat/recommended"],
  ...vueI18n.configs.recommended,
  {
    settings: {
      "vue-i18n": {
        localeDir: "./src/locales/*.{json,yaml,yml}",
        messageSyntaxVersion: "^11.0.0",
      },
    },
  },
  {
    files: ["**/*.vue", "**/*.ts", "**/*.tsx"],
    languageOptions: {
      parser: vueParser,
      parserOptions: {
        parser: tseslint.parser,
        ecmaVersion: "latest",
        sourceType: "module",
        extraFileExtensions: [".vue"],
      },
      globals: {
        window: "readonly",
        document: "readonly",
        navigator: "readonly",
        console: "readonly",
      },
    },
    rules: {
      "@intlify/vue-i18n/no-raw-text": [
        "error",
        {
          attributes: {
            "/.+/": [
              "title",
              "aria-label",
              "aria-placeholder",
              "aria-roledescription",
              "aria-valuetext",
              "label",
              "placeholder",
            ],
            input: ["placeholder"],
            img: ["alt"],
          },
          ignoreNodes: ["md-icon", "v-icon"],
          ignorePattern: "^[-#:\\[\\(\\{\\}\\)\\]+\\/=._\\d\\s]+$",
          ignoreText: ["EUR", "HKD", "USD"],
        },
      ],
      "@intlify/vue-i18n/no-missing-keys": "error",
      "@intlify/vue-i18n/no-unused-keys": ["warn", { extensions: [".vue", ".ts"] }],
      "vue/multi-word-component-names": "off",
    },
  },
  // MUST be last: disable all formatting rules that conflict with Prettier.
  prettierConfig,
];

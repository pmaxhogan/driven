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
      // Every t()/$t() key used in templates and script must exist in a locale
      // file. This is the load-bearing guard against typo'd / dropped keys. The
      // rule only resolves STATIC string keys, so the ~25 dynamic
      // `t(`ns.${var}`)` lookups (see the no-unused-keys ignores below) are
      // simply skipped here - they neither false-positive nor get verified.
      "@intlify/vue-i18n/no-missing-keys": "error",
      // Deprecation guards for the vue-i18n v11 runtime: fail the build if any
      // legacy i18n API creeps back in (<i18n> component, place/places props,
      // the v-t directive, $tc/tc, or the <i18n-t path> prop). None are used
      // today; these keep it that way. They act on templates only, so dynamic
      // keys are irrelevant to them.
      "@intlify/vue-i18n/no-deprecated-i18n-component": "error",
      "@intlify/vue-i18n/no-deprecated-i18n-place-attr": "error",
      "@intlify/vue-i18n/no-deprecated-i18n-places-prop": "error",
      "@intlify/vue-i18n/no-deprecated-tc": "error",
      "@intlify/vue-i18n/no-deprecated-v-t": "error",
      "@intlify/vue-i18n/no-i18n-t-path-prop": "error",
      "vue/multi-word-component-names": "off",
    },
  },
  {
    // Locale RESOURCE files (the JSON/YAML message catalogs). These rules
    // validate message CONTENT and key hygiene; they run against the catalog,
    // not the call sites, so they need their own block (the source-file block
    // above swaps in the Vue parser, which must not touch JSON).
    files: ["src/locales/**/*.{json,json5,yaml,yml}"],
    rules: {
      // Every message must parse as a valid vue-i18n v11 message-format string
      // (balanced {named} interpolations, valid linked @:keys, etc.).
      "@intlify/vue-i18n/valid-message-syntax": "error",
      // Messages are rendered as text, never trusted HTML - forbid raw markup
      // in the catalog so a translation can never become an XSS vector.
      "@intlify/vue-i18n/no-html-messages": "error",
      // A duplicate key silently shadows an earlier value - always a bug.
      "@intlify/vue-i18n/no-duplicate-keys-in-locale": "error",
      // Keep every locale in lock-step: a key present in one catalog must exist
      // in all the others. A no-op today (en-US is the only locale) but it makes
      // adding a second locale safe-by-default instead of silently partial.
      "@intlify/vue-i18n/no-missing-keys-in-other-locales": "error",
      // Pipe-separated plural messages ("a | b | c") must have a valid form
      // count for the locale.
      "@intlify/vue-i18n/valid-plural-forms": "error",
      // Reject the deprecated %{...} modulo interpolation in messages; v11 uses
      // {named} placeholders (which we already do).
      "@intlify/vue-i18n/no-deprecated-modulo-syntax": "error",
      // Flag catalog keys that no source file references. CRITICAL: this rule
      // statically scans t()/$t() call sites and CANNOT see the ~25 keys that
      // the app resolves dynamically via `t(`ns.${var}`)` interpolation (error
      // codes, activity rows, settings dropdowns, nav/tab labels). Every such
      // runtime-resolved namespace is listed in `ignores` below so the rule
      // never reports a live key as "unused" - deleting one of those keys would
      // ship blank UI with no gate to catch it. Kept at "warn" (not "error") as
      // a deliberate safety margin: a genuinely-dead key is surfaced for cleanup
      // without ever blocking CI, and a future namespace missed from `ignores`
      // degrades to a noisy warning rather than a red build that tempts someone
      // into deleting a key that is actually used.
      "@intlify/vue-i18n/no-unused-keys": [
        "warn",
        {
          src: "./src",
          extensions: [".vue", ".ts", ".tsx"],
          // Runtime-resolved key namespaces - referenced ONLY through
          // `t(`...${var}`)` interpolation or `t(variableHoldingTheKey)`, so the
          // static analyzer treats them as unused. Each entry is an anchored
          // regex matched against the full dotted key path. Keep this list in
          // sync with every dynamic lookup in the codebase:
          ignores: [
            // errors.<code>.long  +  errors.<code>.short  (Settings/Activity/
            // About/Restore/SetupWizard/AddSourceWizard/SourceTable/
            // CredentialsWalkthrough + activityEventLabel.ts)
            "/^errors\\./",
            // activity.events.<eventType>  (incl. bracketed "local.*" keys) -
            // activityEventLabel.ts
            "/^activity\\.events\\b/",
            // activity.status.<status>  /  activity.level.<level>  - Activity.vue
            "/^activity\\.status\\./",
            "/^activity\\.level\\./",
            // about.channel.<ch>  - About.vue
            "/^about\\.channel\\./",
            // nav.<link>  - App.vue renders t(link.label) over a literal array
            "/^nav\\./",
            // settings.tabs.<tab>  - Settings.vue renders t(tab.label)
            "/^settings\\.tabs\\./",
            // settings.accounts.state.<state>  - AccountList.vue
            "/^settings\\.accounts\\.state\\./",
            // settings.addSource.step.<step>  - AddSourceWizard.vue
            "/^settings\\.addSource\\.step\\./",
            // settings.rules.* dropdowns - Settings.vue
            "/^settings\\.rules\\.metered\\.mode\\./",
            "/^settings\\.rules\\.ioPriority\\./",
            "/^settings\\.rules\\.vssMode\\./",
            "/^settings\\.rules\\.schedule\\.day\\./",
          ],
        },
      ],
    },
  },
  // MUST be last: disable all formatting rules that conflict with Prettier.
  prettierConfig,
];

// Flat ESLint config for the Driven telemetry Worker (SPEC s16, M9b). Its own
// toolchain - NOT the ui/ eslint setup.
import js from "@eslint/js";
import tseslint from "@typescript-eslint/eslint-plugin";
import tsparser from "@typescript-eslint/parser";

export default [
  {
    ignores: ["node_modules/**", ".wrangler/**", "dist/**"],
  },
  js.configs.recommended,
  {
    files: ["src/**/*.ts", "test/**/*.ts"],
    languageOptions: {
      parser: tsparser,
      parserOptions: {
        ecmaVersion: 2022,
        sourceType: "module",
      },
      globals: {
        Request: "readonly",
        Response: "readonly",
        URL: "readonly",
        JSON: "readonly",
        console: "readonly",
        Number: "readonly",
        Object: "readonly",
        Array: "readonly",
      },
    },
    plugins: {
      "@typescript-eslint": tseslint,
    },
    rules: {
      ...tseslint.configs.recommended.rules,
      "@typescript-eslint/no-explicit-any": "off",
      "no-undef": "off",
    },
  },
];

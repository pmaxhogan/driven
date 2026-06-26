import type { Config } from "tailwindcss";
import colors from "tailwindcss/colors";

export default {
  content: ["./index.html", "./src/**/*.{vue,ts,tsx}"],
  theme: {
    extend: {
      colors: {
        // Brand accent alias -> Tailwind's teal scale. Driven's icon is a white
        // road-to-cloud on deep teal (teal-700 = #0F766E), so teal is THE accent
        // color. Aliasing the whole scale as `brand` means a future rebrand is a
        // one-line change here (point `brand` at a different scale) while every
        // existing `teal-*` utility keeps working unchanged.
        brand: colors.teal,
      },
    },
  },
  plugins: [],
} satisfies Config;

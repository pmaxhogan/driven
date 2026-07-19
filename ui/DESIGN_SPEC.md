# Driven UI design system

This is the single source of truth for the Driven desktop UI's visual language.
Apply these decisions and the EXACT class strings below consistently across every
view and component. Identical strings keep all slices visually coherent and make
a future restyle a find-and-replace rather than a hunt.

## Brand and accent

Driven's icon is a white road-to-cloud on deep teal `#0F766E`, which is exactly
Tailwind `teal-700`. Teal is THE accent color: use it for all primary,
interactive, active, and focus affordances (brand wordmark, primary buttons,
active nav/subtabs, focus rings).

The `@theme` block in `src/style.css` aliases the whole teal scale as `brand`
(`brand-700` === `teal-700`), so a future rebrand is a one-line change (repoint
`--color-brand-*` at a different scale). `teal-*` utilities remain valid and are
what the class strings below use directly.

### Semantic colors (do NOT use teal for these)

- `red-600 / red-700` - destructive actions and errors.
- `amber-*` - warnings.
- `emerald` / `green` - SUCCESS STATUS ONLY (e.g. "done"). Never a primary button.
- `zinc-*` - neutral surfaces, borders, secondary text.

## Foundation (mandatory, lives in `src/style.css`)

The root cause of the early contrast bugs (unreadable dropdowns / invisible text
on a dark-theme OS) was that the app never declared its own background, text
color, or `color-scheme`, so the webview fell back to OS-dark native defaults.
The base layer fixes this and MUST stay:

```css
@layer base {
  :root {
    color-scheme: light dark;
  }
  html,
  body,
  #app {
    @apply bg-zinc-50 text-zinc-900 dark:bg-zinc-950 dark:text-zinc-100;
  }
  input[type="checkbox"],
  input[type="radio"] {
    @apply accent-teal-600;
  }
}
```

Consequence for every other file: each native `<select>` / `<input>` /
`<textarea>` MUST carry an explicit background + text color (the SELECT / TEXT
INPUT string below) and never rely on the browser default.

## Exact class strings

PRIMARY BUTTON:

```
inline-flex items-center justify-center gap-2 rounded-md bg-teal-700 px-4 py-2 text-sm font-medium text-white shadow-xs transition-colors hover:bg-teal-600 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 disabled:cursor-not-allowed disabled:opacity-50
```

SECONDARY BUTTON:

```
inline-flex items-center justify-center gap-2 rounded-md border border-zinc-300 bg-white px-4 py-2 text-sm font-medium text-zinc-700 transition-colors hover:bg-zinc-100 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 disabled:opacity-50 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200 dark:hover:bg-zinc-800
```

DESTRUCTIVE BUTTON: the PRIMARY string with `bg-red-600 hover:bg-red-700` and
`focus-visible:outline-red-500`.

SELECT / TEXT INPUT / SEARCH:

```
rounded-md border border-zinc-300 bg-white px-3 py-2 text-sm text-zinc-900 transition-colors focus:border-teal-500 focus:outline-hidden focus:ring-2 focus:ring-teal-500/40 disabled:opacity-60 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-100
```

CARD / PANEL:

```
rounded-lg border border-zinc-200 bg-white p-4 shadow-xs dark:border-zinc-800 dark:bg-zinc-900
```

Note (Tailwind v4 renames, see `src/style.css`): `focus-visible:outline` (bare,
style-only) is now `focus-visible:outline-solid` because the bare `outline`
utility defaults to setting only `outline-width` in v4; `outline-none` (the
"remove focus ring" idiom) is now `outline-hidden`; `shadow-sm` is now
`shadow-xs` (v4 shifted the shadow scale down a step). These are pure renames -
the rendered pixels are unchanged.

NAV LINK inactive: `text-zinc-600 hover:text-teal-700 dark:text-zinc-400 dark:hover:text-teal-300`

NAV LINK active: `text-teal-700 dark:text-teal-300 font-semibold`

SUBTAB active: `border-b-2 border-teal-600 text-teal-700 font-medium dark:text-teal-300`

EMPTY STATE CARD: `rounded-lg border border-dashed border-zinc-300 p-8 text-center text-sm text-zinc-500 dark:border-zinc-700`

## Empty dropdowns

A `<select>` whose option list is empty MUST render a single disabled,
non-selectable placeholder option that explains WHY it is empty and what to do,
and set `:disabled` on the `<select>` when there are zero real options:

```html
<option value="" disabled>{{ t("activity.filters.noSourcesYet") }}</option>
```

Use messages like "No sources yet - add one in Settings" / "No events logged
yet" so the surface is never a silent dead end.

## Accessibility

- Keep and extend `aria-label`s.
- Teal `focus-visible` rings on every interactive element.
- Mark the active nav item with `aria-current="page"`.
- Preserve every existing `data-testid` (tests depend on them).

## Information architecture (the "duplicate tabs" fix)

The top nav previously listed `Activity | Accounts | Sources | Rules | Restore |
About`, but Accounts / Sources / Rules were ALSO subtabs inside the Settings
page - duplicated and confusing.

Decision:

- Top nav = `[brand "Driven"]  Activity | Settings | Restore | About`. Accounts,
  Sources and Rules are REMOVED from the top nav.
- The "Settings" nav item routes to `/settings`, which renders `Settings.vue`
  with `tab="accounts"` (the default).
- `/accounts`, `/sources` and `/rules` routes are KEPT (tray deep-links + the
  in-page subtabs navigate to them).
- The "Settings" top-nav item shows ACTIVE for any of `/settings`, `/accounts`,
  `/sources`, `/rules`.
- `Settings.vue` keeps its `[Accounts | Sources | Rules]` subtabs - now the ONLY
  place those live - styled with the teal SUBTAB active class.

Ownership: UI-CORE owns `App.vue` (top nav), `router.ts` (the `/settings` route +
active-state logic), and the `nav.settings` i18n key. VIEWS-B owns `Settings.vue`
subtab styling.

## First-run auto-open

On launch, if there are zero configured accounts, the app opens the setup wizard
(`/setup`) instead of the Activity landing; with at least one account, `/`
redirects to `/activity` as normal.

Implementation (`router.ts`): a one-shot `beforeEach` guard self-removes after
the first navigation. It diverts only the default landing (`/` or its `/activity`
redirect target) - deep-links to a specific surface are always honoured. Account
presence is read through the same `list_accounts` IPC command `AccountList.vue`
uses. The guard is robust to IPC failure (any error falls through to `/activity`,
never crashing or trapping the user) and, because it self-removes, can never trap
the user on `/setup` once they have an account or navigate away.

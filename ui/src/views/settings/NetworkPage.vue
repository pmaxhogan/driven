<script setup lang="ts">
import { ref, watch } from "vue";
import { useI18n } from "vue-i18n";

import { useSettingsStore } from "../../stores/settings";
import { useSettingsForm } from "../../composables/useSettingsForm";
import { validateCustomCa, validateProxy } from "../../ipc/commands";
import { cardCls, inputCls, ensureSettingsLoaded } from "./shared";

// Network settings page (SDD 2026-08-02 settings-sidebar-ia, task 2). Moved
// verbatim out of Settings.vue: the custom corporate root CA card and the
// proxy (SOCKS5 + PAC) card (issue #34).
const { t } = useI18n();
const settings = useSettingsStore();
const { commitPatch } = useSettingsForm();

ensureSettingsLoaded();

// Issue #34: custom corporate root CA path + inline validation feedback.
const customCaPath = ref("");
const caFeedback = ref<{ ok: boolean; message: string } | null>(null);

// Issue #34: proxy mode + URL/PAC source + inline validation feedback.
const proxyModes = ["system", "none", "manual", "pac"] as const;
const proxyMode = ref("system");
const proxyUrl = ref("");
const proxyFeedback = ref<{ ok: boolean; message: string } | null>(null);

watch(
  () => settings.settings,
  (s) => {
    if (!s) return;
    customCaPath.value = s.global.customRootCaPath ?? "";
    // NOTE: do NOT reset `caFeedback` here - `commitCustomCa` updates the store,
    // which re-runs this loader, and clearing it would wipe the just-shown
    // validation result. `caFeedback` is owned solely by `commitCustomCa`.
    proxyMode.value = s.global.proxyMode ?? "system";
    proxyUrl.value = s.global.proxyUrl ?? "";
    // As with `caFeedback`, `proxyFeedback` is owned solely by `commitProxy`.
  },
  { immediate: true }
);

// Issue #34: save the custom root CA path. A blank value clears it (system trust
// only). A non-blank value is validated against the backend FIRST (which parses
// the PEM) so the user gets an explicit cert-count / parse-error result and a
// broken path is never persisted; only a valid file is committed.
async function commitCustomCa(): Promise<void> {
  const path = customCaPath.value.trim();
  if (path === "") {
    caFeedback.value = null;
    await commitPatch({ global: { customRootCaPath: null } });
    return;
  }
  try {
    const res = await validateCustomCa(path);
    caFeedback.value = {
      ok: true,
      message: t("settings.rules.customCa.validCount", { count: res.certCount }),
    };
  } catch {
    // Do NOT persist an unparseable path (it would fail-closed every connection).
    caFeedback.value = { ok: false, message: t("settings.rules.customCa.invalid") };
    return;
  }
  await commitPatch({ global: { customRootCaPath: path } });
}

// Issue #34: change the proxy mode. system/none need no URL and commit straight
// away (clearing the URL); manual/pac defer the commit to `commitProxy` once a
// URL is entered (so we never persist a manual/pac mode with no proxy).
async function setProxyMode(event: Event): Promise<void> {
  const mode = (event.target as HTMLSelectElement).value;
  proxyMode.value = mode;
  proxyFeedback.value = null;
  if (mode === "system" || mode === "none") {
    await commitPatch({ global: { proxyMode: mode, proxyUrl: null } });
  }
  // manual / pac: wait for the URL field (commitProxy). A pre-existing valid URL
  // (loaded from the store) is re-validated + committed when the user leaves the
  // URL field, or immediately here if one is already present.
  else if (proxyUrl.value.trim() !== "") {
    await commitProxy();
  }
}

// Issue #34: validate + save the proxy URL / PAC source for manual/pac mode. The
// backend validation parses a manual URL or fetches + compiles a PAC file FIRST,
// so a broken proxy is never persisted (which would fail-closed every outbound
// connection); only a usable value is committed.
async function commitProxy(): Promise<void> {
  const mode = proxyMode.value;
  if (mode === "system" || mode === "none") {
    proxyFeedback.value = null;
    await commitPatch({ global: { proxyMode: mode, proxyUrl: null } });
    return;
  }
  const url = proxyUrl.value.trim();
  if (url === "") {
    proxyFeedback.value = { ok: false, message: t("settings.rules.proxy.requiresUrl") };
    return;
  }
  try {
    await validateProxy(mode, url);
    proxyFeedback.value = { ok: true, message: t("settings.rules.proxy.valid") };
  } catch {
    // Do NOT persist an unusable proxy (it would fail-closed every connection).
    proxyFeedback.value = { ok: false, message: t("settings.rules.proxy.invalid") };
    return;
  }
  await commitPatch({ global: { proxyMode: mode, proxyUrl: url } });
}
</script>

<template>
  <p v-if="settings.loading && !settings.settings" class="text-sm text-zinc-500">
    {{ t("common.loading") }}
  </p>
  <p v-else-if="!settings.settings && settings.errorCode" class="text-sm text-red-600" role="alert">
    {{ t(`errors.${settings.errorCode}.long`) }}
  </p>
  <div v-else-if="settings.settings" class="max-w-2xl space-y-4 text-sm" data-testid="rules-form">
    <p
      v-if="settings.errorCode"
      class="rounded-md bg-red-50 px-3 py-2 text-sm text-red-700 dark:bg-red-950/40 dark:text-red-300"
      role="alert"
      data-testid="rules-error"
    >
      {{ t(`errors.${settings.errorCode}.long`) }}
    </p>

    <!-- Issue #34: custom corporate root CA -->
    <section class="space-y-2" :class="cardCls" data-testid="custom-ca-setting">
      <h3 class="text-sm font-semibold text-zinc-800 dark:text-zinc-200">
        {{ t("settings.rules.customCa.title") }}
      </h3>
      <label class="block space-y-1">
        <span class="text-zinc-600 dark:text-zinc-400">{{
          t("settings.rules.customCa.label")
        }}</span>
        <input
          v-model="customCaPath"
          type="text"
          data-testid="custom-ca-path"
          class="w-full font-mono"
          :class="inputCls"
          :placeholder="t('settings.rules.customCa.placeholder')"
          @change="commitCustomCa"
        />
      </label>
      <p
        v-if="caFeedback"
        data-testid="custom-ca-feedback"
        class="text-xs"
        :class="
          caFeedback.ok ? 'text-teal-600 dark:text-teal-400' : 'text-red-600 dark:text-red-400'
        "
      >
        {{ caFeedback.message }}
      </p>
      <p class="text-xs text-zinc-500">
        {{ t("settings.rules.customCa.note") }}
      </p>
    </section>

    <!-- Issue #34: proxy (SOCKS5 + PAC) -->
    <section class="space-y-2" :class="cardCls" data-testid="proxy-setting">
      <h3 class="text-sm font-semibold text-zinc-800 dark:text-zinc-200">
        {{ t("settings.rules.proxy.title") }}
      </h3>
      <label class="block space-y-1">
        <span class="text-zinc-600 dark:text-zinc-400">{{
          t("settings.rules.proxy.modeLabel")
        }}</span>
        <select
          data-testid="proxy-mode"
          class="w-full"
          :class="inputCls"
          :value="proxyMode"
          @change="setProxyMode"
        >
          <option v-for="mode in proxyModes" :key="mode" :value="mode">
            {{ t(`settings.rules.proxy.mode.${mode}`) }}
          </option>
        </select>
      </label>
      <label v-if="proxyMode === 'manual' || proxyMode === 'pac'" class="block space-y-1">
        <span class="text-zinc-600 dark:text-zinc-400">{{
          proxyMode === "pac"
            ? t("settings.rules.proxy.pacLabel")
            : t("settings.rules.proxy.urlLabel")
        }}</span>
        <input
          v-model="proxyUrl"
          type="text"
          data-testid="proxy-url"
          class="w-full font-mono"
          :class="inputCls"
          :placeholder="
            proxyMode === 'pac'
              ? t('settings.rules.proxy.pacPlaceholder')
              : t('settings.rules.proxy.urlPlaceholder')
          "
          @change="commitProxy"
        />
      </label>
      <p
        v-if="proxyFeedback"
        data-testid="proxy-feedback"
        class="text-xs"
        :class="
          proxyFeedback.ok ? 'text-teal-600 dark:text-teal-400' : 'text-red-600 dark:text-red-400'
        "
      >
        {{ proxyFeedback.message }}
      </p>
      <p class="text-xs text-zinc-500">
        {{ t("settings.rules.proxy.note") }}
      </p>
    </section>
  </div>
</template>

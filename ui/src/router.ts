import { createRouter, createWebHistory, type RouteRecordRaw } from "vue-router";

// SPEC s25 route map. The Settings view hosts the Accounts / Sources / Rules
// tabs (DESIGN s8.2); each tab has its own path so the tray menu + deep links
// can target a specific tab directly. Activity (M7) and Restore (M8) are
// placeholder views in M6 (the "coming in M7/M8" shells), so the routes exist
// and resolve but render a placeholder until those milestones.
const routes: RouteRecordRaw[] = [
  {
    path: "/setup",
    name: "setup",
    component: () => import("./views/SetupWizard.vue"),
  },
  {
    // SPEC s25: "/" redirects to /activity.
    path: "/",
    redirect: "/activity",
  },
  {
    path: "/activity",
    name: "activity",
    component: () => import("./views/Activity.vue"),
  },
  {
    path: "/accounts",
    name: "accounts",
    component: () => import("./views/Settings.vue"),
    props: { tab: "accounts" },
  },
  {
    path: "/sources",
    name: "sources",
    component: () => import("./views/Settings.vue"),
    props: { tab: "sources" },
  },
  {
    path: "/rules",
    name: "rules",
    component: () => import("./views/Settings.vue"),
    props: { tab: "rules" },
  },
  {
    path: "/about",
    name: "about",
    component: () => import("./views/About.vue"),
  },
  {
    path: "/restore",
    name: "restore",
    component: () => import("./views/Restore.vue"),
  },
  {
    path: "/restore/:sourceId",
    name: "restore-scoped",
    component: () => import("./views/Restore.vue"),
    props: true,
  },
];

export const router = createRouter({
  history: createWebHistory(),
  routes,
});

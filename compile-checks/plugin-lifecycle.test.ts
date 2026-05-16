// Compile-time probe for the React lifecycle module shipped in Round 10
// (task7). Mirrors the canonical hook flow from
// docs/specs/plugin-contract.md §"Lifecycle Hooks (Frontend)":
//
//   const capability = usePluginCapability(pluginId, mountId);
//   if (capability.status === "loading") return <LoadingState />;
//   if (capability.status === "error") return <ErrorState ... />;
//   return <PluginApp capability={capability.value} />;
//
// We exercise:
//   - the discriminated-union narrowing on `status`,
//   - the brand on the capability handle (Round-6 `PluginCapability =
//     string & { readonly __brand: "PluginCapability" }`),
//   - the required-positional-arg shape of usePluginCapability.

import type {
  PluginCapabilityState,
  PluginCapabilityValue,
} from "../src/plugin-lifecycle";
import { usePluginCapability } from "../src/plugin-lifecycle";
import type { PluginCapability } from "../src/generated/capability";

export function _probe_capability_value(value: PluginCapabilityValue): string {
  // Field shape: { pluginId, mountId, capability, generation }
  const { pluginId, mountId, capability, generation } = value;
  return `${pluginId}|${mountId}|${capability}|${generation}`;
}

export function _probe_state_discrimination(state: PluginCapabilityState): string {
  switch (state.status) {
    case "loading":
      return "loading";
    case "ready":
      // After narrowing, `value` is available.
      return `ready:${state.value.mountId}`;
    case "error":
      return `error:${state.error.kind}`;
  }
}

export function _probe_brand_resists_plain_string(): void {
  function takesCapability(_c: PluginCapability): void {
    void _c;
  }
  // @ts-expect-error plain string is not branded as PluginCapability
  takesCapability("just-a-string");
}

export function _probe_use_plugin_capability_requires_plugin_id(): void {
  // @ts-expect-error pluginId is required positional argument
  usePluginCapability();
  // Calling with just pluginId is the documented minimal shape.
  usePluginCapability("example-notes");
  // tabId is optional.
  usePluginCapability("example-notes", "tab-example-notes");
}

export function _probe_loading_state_has_no_value(): void {
  const state: PluginCapabilityState = { status: "loading" };
  if (state.status === "loading") {
    // @ts-expect-error `value` is not present on the loading variant
    state.value;
  }
}

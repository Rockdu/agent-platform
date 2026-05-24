// Frontend scaffold for the papers plugin.
//
// Proves the manifest-declared frontend loads in the host shell via
// PluginRoot, mints a capability via mountPlugin, and exposes it via
// usePluginCapabilityValue(). The digest panel, manual search box,
// and read/star controls plug in here as the feature lands.

import { usePluginCapabilityValue } from "../../../src/plugin-lifecycle";

const FIXTURE_BANNER = "papers-plugin-scaffold-loaded";

export default function PapersPanel() {
  const capability = usePluginCapabilityValue();

  const handleEnvelopePrefix = (() => {
    const parts = capability.capability.split(".");
    if (parts.length < 2 || parts[0] !== "cap_v1") return "cap_v1.…";
    return `cap_v1.${parts[1].slice(0, 8)}…`;
  })();

  return (
    <section className="placeholder" data-fixture={FIXTURE_BANNER}>
      <h2>论文</h2>
      <p>
        Papers 插件骨架已加载。每日 10:00 arXiv 推荐、手动搜索、收藏与已读状态将在后续接入。
      </p>
      <dl className="plugin-capability-summary">
        <dt>mount_id</dt>
        <dd>
          <code>{capability.mountId}</code>
        </dd>
        <dt>handle 前缀</dt>
        <dd>
          <code>{handleEnvelopePrefix}</code>
        </dd>
        <dt>generation</dt>
        <dd>
          <code>{capability.generation}</code>
        </dd>
      </dl>
    </section>
  );
}

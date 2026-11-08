import { useId, useState } from 'react';
import { createPortal } from 'react-dom';
import { api } from '@/api';
import { effectiveLabel } from '@/api/decode';
import type { CostRevision, ModelPrice, PriceRevision, Settings, SubscriptionCosts } from '@/api/types';
import { ApiPrices } from '@/components/costs/ApiPrices';
import { PeakBand } from '@/components/costs/PeakBand';
import { PlanRates } from '@/components/costs/PlanRates';
import { WindowsByTier } from '@/components/costs/WindowsByTier';
import { clock } from '@/lib/clock';
import { revisionAt } from '@/lib/costs';
import { useAsync, useNow } from '@/lib/hooks';
import { MINUTE } from '@/lib/time';
import { Button, Dialog, ErrorState, Select, Skeleton, Tabs } from './primitives';

export type CostTab = 'api-prices' | 'plan-rates' | 'tiers';

const TABS: { label: string; value: CostTab }[] = [
  { label: 'Tiers and peak band', value: 'tiers' },
  { label: 'Plan rates', value: 'plan-rates' },
  { label: 'API prices', value: 'api-prices' },
];

interface Dated {
  effective: string;
  effectiveDate: string | null;
}

/**
 * Both files' revision dates, one entry per date, ascending as each file lists its own. An entry dated in free text
 * anchors no instant, so choosing it would resolve to none and the select would snap back: it stays off the list.
 */
const unionOf = (settings: Settings): Dated[] => {
  const byDate = new Map<string, string | null>();
  for (const r of [...settings.apiPrices.revisions, ...settings.subscriptionCosts.revisions]) if (r.effectiveDate !== null) byDate.set(r.effective, r.effectiveDate);
  return [...byDate].map(([effective, effectiveDate]) => ({ effective, effectiveDate })).sort((a, b) => (a.effectiveDate ?? a.effective).localeCompare(b.effectiveDate ?? b.effective));
};

/** The "Cost reference" link, whose dialog opens on `tab` and holds the page's auto reload while open. */
export function CostReferenceLink({ tab }: { tab: CostTab }) {
  const [open, setOpen] = useState(false);
  const [shown, setShown] = useState<CostTab>(tab);
  // The chosen revision date, null for the entries in force at the opening.
  const [revision, setRevision] = useState<string | null>(null);
  const [openedAt, setOpenedAt] = useState(0);
  // Read on every opening, so the dialog shows the tables in force, and never while it is closed.
  const settings = useAsync(() => (open ? api.getSettings() : Promise.resolve(null)), [open]);
  const close = () => setOpen(false);
  const s = settings.data;
  const union = s === null ? [] : unionOf(s);
  const chosen = union.find((r) => r.effective === revision)?.effectiveDate ?? null;
  // One instant drives the select, both tables and both chips, so none of them can disagree on the entry in force.
  const at = chosen === null ? openedAt : Date.parse(chosen);
  const selected = revisionAt(union, at)?.effective ?? null;
  const pricesAt = s === null ? null : revisionAt(s.apiPrices.revisions, at);
  const costsAt = s === null ? null : revisionAt(s.subscriptionCosts.revisions, at);
  const pending = (r: Dated) => r.effectiveDate !== null && Date.parse(r.effectiveDate) > openedAt;
  return (
    <>
      <button
        type="button"
        className="link-btn"
        onClick={() => {
          setShown(tab);
          setRevision(null);
          setOpenedAt(clock.now());
          setOpen(true);
        }}
      >
        Cost reference
      </button>
      {/* A link may sit in a card's subtitle paragraph, where a dialog cannot nest, so the dialog lives under the body. */}
      {createPortal(
        <Dialog
          open={open}
          onClose={close}
          title="Cost reference"
          subtitle="What a million tokens costs: in dollars at list price, and as a share of a subscription window."
          width={1200}
          className="dialog--costs dialog--fixed"
          footer={
            <>
              {union.length > 1 && (
                <label className="row cost-ref__revision">
                  <span className="muted">Revision</span>
                  <Select aria-label="Revision" value={selected ?? ''} onChange={(e) => setRevision(e.target.value)}>
                    {[...union].reverse().map((r) => (
                      <option key={r.effective} value={r.effective}>
                        {`${effectiveLabel(r.effective)}${pending(r) ? ' (next)' : ''}`}
                      </option>
                    ))}
                  </Select>
                </label>
              )}
              <Provenance costs={costsAt} prices={pricesAt} settings={s} />
              <Button variant="primary" onClick={close}>
                Close
              </Button>
            </>
          }
        >
          <CostPanels
            costs={s === null ? null : (costsAt?.content ?? s.subscriptionCosts)}
            error={settings.error}
            onTab={setShown}
            prices={s === null ? null : (pricesAt?.models ?? s.apiPrices.models)}
            problem={s?.subscriptionCosts.problem ?? null}
            tab={shown}
          />
        </Dialog>,
        document.body,
      )}
    </>
  );
}

/**
 * The three tabs, each kept mounted while another is shown, so a tab's controls hold their state for the opening. The
 * tables are the entries in force at the dialog's instant, the cost model's problem the envelope's.
 */
function CostPanels({ costs, error, onTab, prices, problem, tab }: { costs: SubscriptionCosts | null; error: string | null; onTab: (tab: CostTab) => void; prices: ModelPrice[] | null; problem: string | null; tab: CostTab }) {
  const now = useNow(MINUTE);
  const id = useId();
  // Both model tables fold their older models together, and a new opening mounts the panels folded again.
  const [olderOpen, setOlderOpen] = useState(false);
  const onToggleOlder = () => setOlderOpen((v) => !v);
  const body = (value: CostTab) => {
    if (error !== null) return <ErrorState title="Could not load the cost tables" body={error} />;
    if (costs === null || prices === null) return <Skeleton height={240} />;
    if (value !== 'api-prices' && problem !== null) return <ErrorState title={problem} />;
    if (value === 'tiers') {
      return (
        <>
          <WindowsByTier costs={costs} />
          <PeakBand costs={costs} now={now} />
        </>
      );
    }
    return value === 'plan-rates' ? (
      <PlanRates costs={costs} now={now} olderOpen={olderOpen} onToggleOlder={onToggleOlder} prices={prices} />
    ) : (
      <ApiPrices costs={costs} olderOpen={olderOpen} onToggleOlder={onToggleOlder} prices={prices} />
    );
  };
  return (
    <div className="cost-ref">
      <div className="cost-ref__tabs">
        <Tabs<CostTab> label="Cost reference" value={tab} onChange={onTab} tabs={TABS.map((t) => ({ ...t, controls: `${id}-${t.value}-panel`, id: `${id}-${t.value}` }))} />
      </div>
      {TABS.map((t) => (
        <section key={t.value} id={`${id}-${t.value}-panel`} className="cost-ref__panel" role="tabpanel" aria-labelledby={`${id}-${t.value}`} hidden={t.value !== tab}>
          {body(t.value)}
        </section>
      ))}
    </div>
  );
}

/** Where each table drawn came from: its entry's date and the file's source, the key and path that would replace it in
 *  the title. */
function Provenance({ costs, prices, settings }: { costs: CostRevision | null; prices: PriceRevision | null; settings: Settings | null }) {
  if (settings === null) return <div className="cost-ref__prov" />;
  const chip = (label: string, effective: string, source: string, key: string, path: string | null) => (
    <span className="chip" title={source === 'shipped' ? `The table this release ships. A copy at ${key}${path === null ? '' : ` (${path})`} would replace it at the next start.` : `Read from ${path ?? key}, set by ${key}.`}>
      {label}, effective {effectiveLabel(effective)}, {source}
    </span>
  );
  return (
    <div className="cost-ref__prov">
      {chip('API prices', prices?.effective ?? settings.apiPrices.effective, settings.apiPrices.source, 'stats.api_prices_path', settings.storage.apiPricesPath)}
      {settings.subscriptionCosts.problem === null && chip('Subscription cost model', costs?.effective ?? settings.subscriptionCosts.effective, settings.subscriptionCosts.source, 'stats.subscription_costs_path', settings.storage.subscriptionCostsPath)}
    </div>
  );
}

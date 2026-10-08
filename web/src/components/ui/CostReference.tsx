import { useId, useState } from 'react';
import { createPortal } from 'react-dom';
import { api } from '@/api';
import { effectiveLabel } from '@/api/decode';
import type { Settings } from '@/api/types';
import { ApiPrices } from '@/components/costs/ApiPrices';
import { PeakBand } from '@/components/costs/PeakBand';
import { PlanRates } from '@/components/costs/PlanRates';
import { WindowsByTier } from '@/components/costs/WindowsByTier';
import { useAsync, useNow } from '@/lib/hooks';
import { Button, Dialog, ErrorState, Skeleton, Tabs } from './primitives';

export type CostTab = 'api-prices' | 'plan-rates' | 'tiers';

const TABS: { label: string; value: CostTab }[] = [
  { label: 'Tiers and peak band', value: 'tiers' },
  { label: 'Plan rates', value: 'plan-rates' },
  { label: 'API prices', value: 'api-prices' },
];

/** The "Cost reference" link; the dialog opens on `tab`. */
export function CostReferenceLink({ tab }: { tab: CostTab }) {
  const [open, setOpen] = useState(false);
  const [shown, setShown] = useState<CostTab>(tab);
  // Read on every opening, so the dialog shows the tables in force, and never while it is closed.
  const settings = useAsync(() => (open ? api.getSettings() : Promise.resolve(null)), [open]);
  const close = () => setOpen(false);
  return (
    <>
      <button
        type="button"
        className="link-btn"
        onClick={() => {
          setShown(tab);
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
          className="dialog--costs"
          footer={
            <>
              <Provenance settings={settings.data} />
              <Button variant="primary" onClick={close}>
                Close
              </Button>
            </>
          }
        >
          {open && <CostPanels error={settings.error} onTab={setShown} settings={settings.data} tab={shown} />}
        </Dialog>,
        document.body,
      )}
    </>
  );
}

/** The three tabs, each kept mounted while another is shown, so a tab's controls hold their state for the opening. */
function CostPanels({ error, onTab, settings, tab }: { error: string | null; onTab: (tab: CostTab) => void; settings: Settings | null; tab: CostTab }) {
  const now = useNow(60_000);
  const id = useId();
  const body = (value: CostTab) => {
    if (error !== null) return <ErrorState title="Could not load the cost tables" body={error} />;
    if (settings === null) return <Skeleton height={240} />;
    const costs = settings.subscriptionCosts;
    if (value !== 'api-prices' && costs.problem !== null) return <ErrorState title={costs.problem} />;
    if (value === 'tiers') {
      return (
        <>
          <WindowsByTier costs={costs} />
          <PeakBand costs={costs} now={now} />
        </>
      );
    }
    return value === 'plan-rates' ? <PlanRates costs={costs} now={now} prices={settings.apiPrices.models} /> : <ApiPrices prices={settings.apiPrices.models} />;
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

/** Where each table in force came from: its date and source, the key and path that would replace it in the title. */
function Provenance({ settings }: { settings: Settings | null }) {
  if (settings === null) return <div className="cost-ref__prov" />;
  const chip = (label: string, effective: string, source: string, key: string, path: string | null) => (
    <span className="chip" title={source === 'shipped' ? `The table this release ships. A copy at ${key}${path === null ? '' : ` (${path})`} would replace it at the next start.` : `Read from ${path ?? key}, set by ${key}.`}>
      {label}, effective {effectiveLabel(effective)}, {source}
    </span>
  );
  return (
    <div className="cost-ref__prov">
      {chip('API prices', settings.apiPrices.effective, settings.apiPrices.source, 'stats.api_prices_path', settings.storage.apiPricesPath)}
      {settings.subscriptionCosts.problem === null && chip('Subscription cost model', settings.subscriptionCosts.effective, settings.subscriptionCosts.source, 'stats.subscription_costs_path', settings.storage.subscriptionCostsPath)}
    </div>
  );
}

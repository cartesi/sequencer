export type MonitorDebugOptions = {
  includeBytes: boolean;
  includeSigs: boolean;
  includeDetails: boolean;
  limitEnvelopes: number;
};

type Props = {
  value: MonitorDebugOptions;
  onChange: (next: MonitorDebugOptions) => void;
};

export function DebugControls({ value, onChange }: Props) {
  return (
    <section className="card">
      <h3>Debug Controls</h3>
      <div className="field-grid compact">
        <label>
          <input
            type="checkbox"
            checked={value.includeBytes}
            onChange={(event) => onChange({ ...value, includeBytes: event.target.checked })}
          />
          includeBytes
        </label>

        <label>
          <input
            type="checkbox"
            checked={value.includeSigs}
            onChange={(event) => onChange({ ...value, includeSigs: event.target.checked })}
          />
          includeSigs
        </label>

        <label>
          <input
            type="checkbox"
            checked={value.includeDetails}
            onChange={(event) => onChange({ ...value, includeDetails: event.target.checked })}
          />
          includeDetails (pending)
        </label>

        <label>
          limitEnvelopes
          <input
            type="number"
            min={1}
            value={value.limitEnvelopes}
            onChange={(event) => {
              const parsed = Number.parseInt(event.target.value, 10);
              onChange({
                ...value,
                limitEnvelopes: Number.isFinite(parsed) && parsed > 0 ? parsed : 1,
              });
            }}
          />
        </label>
      </div>
    </section>
  );
}

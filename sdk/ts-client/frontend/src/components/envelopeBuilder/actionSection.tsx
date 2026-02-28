import type { Hex } from "viem";

import {
  buildCancelActionBytes,
  buildPlaceOrderActionBytes,
  buildWithdrawActionBytes,
  parseBigIntInput,
} from "../../eip712/builders";
import type { ActionType } from "./types";

export type ActionFormValues = {
  symbol: string;
  price: string;
  priceExp: string;
  size: string;
  sizeExp: string;
  side: string;
  tif: string;
  postOnly: string;
  reduceOnly: string;
  orderId: string;
  amount: string;
  amountExp: string;
};

type ActionTypeSectionProps = {
  actionType: ActionType;
  values: ActionFormValues;
  onFieldChange: <K extends keyof ActionFormValues>(field: K, value: string) => void;
};

type FieldSpec = {
  key: keyof ActionFormValues;
  label: string;
};

const PLACE_FIELDS: FieldSpec[] = [
  { key: "symbol", label: "symbol" },
  { key: "price", label: "price" },
  { key: "priceExp", label: "priceExp" },
  { key: "size", label: "size" },
  { key: "sizeExp", label: "sizeExp" },
  { key: "side", label: "side" },
  { key: "tif", label: "tif" },
  { key: "postOnly", label: "postOnly" },
  { key: "reduceOnly", label: "reduceOnly" },
];

const CANCEL_FIELDS: FieldSpec[] = [{ key: "orderId", label: "orderId" }];

const WITHDRAW_FIELDS: FieldSpec[] = [
  { key: "amount", label: "amount" },
  { key: "amountExp", label: "amountExp" },
];

export function ActionTypeSection({ actionType, values, onFieldChange }: ActionTypeSectionProps) {
  if (actionType === "place") {
    return <FieldGrid fields={PLACE_FIELDS} values={values} onFieldChange={onFieldChange} />;
  }
  if (actionType === "cancel") {
    return <FieldGrid fields={CANCEL_FIELDS} values={values} onFieldChange={onFieldChange} />;
  }
  return <FieldGrid fields={WITHDRAW_FIELDS} values={values} onFieldChange={onFieldChange} />;
}

function FieldGrid({
  fields,
  values,
  onFieldChange,
}: {
  fields: FieldSpec[];
  values: ActionFormValues;
  onFieldChange: <K extends keyof ActionFormValues>(field: K, value: string) => void;
}) {
  return (
    <div className="field-grid">
      {fields.map((field) => (
        <label key={field.key}>
          {field.label}
          <input value={values[field.key]} onChange={(event) => onFieldChange(field.key, event.target.value)} />
        </label>
      ))}
    </div>
  );
}

export function buildActionBytesFromValues(actionType: ActionType, values: ActionFormValues): Hex {
  if (actionType === "place") {
    return buildPlaceOrderActionBytes({
      symbol: parseBigIntInput(values.symbol, "symbol"),
      price: parseBigIntInput(values.price, "price"),
      priceExp: parseBigIntInput(values.priceExp, "priceExp"),
      size: parseBigIntInput(values.size, "size"),
      sizeExp: parseBigIntInput(values.sizeExp, "sizeExp"),
      side: parseBigIntInput(values.side, "side"),
      tif: parseBigIntInput(values.tif, "tif"),
      postOnly: parseBigIntInput(values.postOnly, "postOnly"),
      reduceOnly: parseBigIntInput(values.reduceOnly, "reduceOnly"),
    });
  }
  if (actionType === "cancel") {
    return buildCancelActionBytes({
      orderId: parseBigIntInput(values.orderId, "orderId"),
    });
  }
  return buildWithdrawActionBytes({
    amount: parseBigIntInput(values.amount, "amount"),
    amountExp: parseBigIntInput(values.amountExp, "amountExp"),
  });
}

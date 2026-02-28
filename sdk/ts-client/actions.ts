export const ACTION_WITHDRAW = 2;
export const ACTION_PLACE = 3;
export const ACTION_CANCEL = 4;

export const ACTION_LENGTH_BY_TYPE: Readonly<Record<number, number>> = {
  [ACTION_WITHDRAW]: 10,
  [ACTION_PLACE]: 31,
  [ACTION_CANCEL]: 9,
};

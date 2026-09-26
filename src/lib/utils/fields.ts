/** The backend flattens provider fields onto the object; `extra` only exists on objects built here. */
export function extraField<T = unknown>(item: object | null | undefined, key: string): T | undefined {
  if (!item) return undefined;
  const direct = (item as Record<string, unknown>)[key];
  if (direct !== undefined && direct !== null) return direct as T;
  const nested = (item as { extra?: Record<string, unknown> }).extra?.[key];
  return nested === undefined || nested === null ? undefined : (nested as T);
}

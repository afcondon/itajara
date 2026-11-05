export const envImpl = (name) => () => {
  const v = typeof process !== "undefined" ? process.env[name] : undefined;
  return v === undefined || v === "" ? null : v;
};

export const stampImpl = () => new Date().toISOString().slice(11, 23);

export const onSignalImpl = (k) => () => {
  if (typeof process === "undefined") return;
  const stop = () => { k(); setTimeout(() => process.exit(0), 300).unref(); };
  process.on("SIGTERM", stop);
  process.on("SIGINT", stop);
};

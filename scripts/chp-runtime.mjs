export function validateNodeMajor(version, requiredMajor = 20) {
  const match = /^v?(\d+)\./.exec(version);
  const major = match ? Number.parseInt(match[1], 10) : Number.NaN;
  if (major !== requiredMajor) {
    throw new Error(`CHP oracle requires Node ${requiredMajor}, got ${version}`);
  }
}

export function staleResumeRequiresRefresh(input: {
  hardReset: boolean;
  seenRuntimeInstanceId: string | null;
  resumeRuntimeInstanceId: string;
}): boolean {
  return (
    input.hardReset ||
    (input.seenRuntimeInstanceId !== null &&
      input.seenRuntimeInstanceId !== input.resumeRuntimeInstanceId)
  );
}

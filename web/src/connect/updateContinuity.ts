export interface PairingContinuity {
  pairingCodeGeneration: number;
  hostIdentityFingerprint: string;
  deviceKeyFingerprint: string;
}

export interface UpdateContinuityState {
  protocolMajor: number;
  protocolMinor: number;
  bundleId: string;
  pairing: PairingContinuity;
  localDraft: string | null;
  mutationsPaused: boolean;
  reloadRequired: boolean;
}

export function createUpdateContinuity(
  bundleId: string,
  pairing: PairingContinuity,
  protocolMajor: number,
  protocolMinor: number,
): UpdateContinuityState {
  return {
    protocolMajor,
    protocolMinor,
    bundleId,
    pairing,
    localDraft: null,
    mutationsPaused: false,
    reloadRequired: false,
  };
}

export function observePeerBundle(
  state: UpdateContinuityState,
  protocolMajor: number,
  protocolMinor: number,
  bundleId: string,
): UpdateContinuityState {
  if (
    protocolMajor !== state.protocolMajor ||
    protocolMinor > state.protocolMinor ||
    bundleId !== state.bundleId
  ) {
    return {
      ...state,
      mutationsPaused: true,
      reloadRequired: true,
    };
  }
  return state;
}

export function pairingRotated(
  current: PairingContinuity,
  observed: PairingContinuity,
): boolean {
  return (
    current.pairingCodeGeneration !== observed.pairingCodeGeneration ||
    current.hostIdentityFingerprint !== observed.hostIdentityFingerprint
  );
}

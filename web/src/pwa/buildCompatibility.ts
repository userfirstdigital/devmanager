declare const __DEVMANAGER_WEB_BUILD_ID__: string;

export const CLIENT_WEB_BUILD_ID = __DEVMANAGER_WEB_BUILD_ID__;

export type BuildCompatibility =
  | { kind: "compatible" }
  | {
      kind: "reloadRequired";
      clientBuildId: string;
      hostBuildId: string;
    };

export function evaluateBuildCompatibility(
  hostBuildId: string,
): BuildCompatibility {
  if (hostBuildId === CLIENT_WEB_BUILD_ID) return { kind: "compatible" };
  return {
    kind: "reloadRequired",
    clientBuildId: CLIENT_WEB_BUILD_ID,
    hostBuildId,
  };
}

async function invokeFetchSttModels(
  url: string,
  apiKey: string,
): Promise<string[]> {
  const { invoke } = await import("@tauri-apps/api/core");
  const args: Record<string, unknown> = { url };
  if (apiKey) {
    args.token = apiKey;
  }
  return await invoke("fetch_stt_models", args);
}

export async function listSttModels(
  baseUrl: string,
  apiKey: string,
): Promise<string[]> {
  const base = baseUrl.replace(/\/+$/, "");
  const url = base.match(/\/v\d+$/) ? `${base}/models` : `${base}/v1/models`;

  try {
    return await invokeFetchSttModels(url, apiKey);
  } catch (err) {
    const msg = `fetch ${url} failed: ${err}`;
    throw new Error(msg);
  }
}

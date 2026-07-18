import { Effect, pipe, Schema } from "effect";

import {
  DEFAULT_RESULT,
  fetchJson,
  type ListModelsResult,
  REQUEST_TIMEOUT,
} from "./list-common";

const SttModelSchema = Schema.Struct({
  data: Schema.Array(
    Schema.Struct({
      id: Schema.String,
      task: Schema.optional(Schema.String),
    }),
  ),
});

const STT_KEYWORDS = ["transcribe", "whisper", "speech", "audio"] as const;

function isSttModel(id: string, task?: string): boolean {
  if (task && task === "automatic-speech-recognition") {
    return true;
  }

  const lowerId = id.toLowerCase();
  return STT_KEYWORDS.some((keyword) => lowerId.includes(keyword));
}

export async function listSttModels(
  baseUrl: string,
  apiKey: string,
): Promise<ListModelsResult> {
  if (!baseUrl) {
    return DEFAULT_RESULT;
  }

  return pipe(
    fetchJson(`${baseUrl}/models`, { Authorization: `Bearer ${apiKey}` }),
    Effect.andThen((json) => Schema.decodeUnknown(SttModelSchema)(json)),
    Effect.map(({ data }) => ({
      models: data
        .filter((model) => isSttModel(model.id, model.task))
        .map((model) => model.id),
      ignored: [],
      metadata: {},
    })),
    Effect.timeout(REQUEST_TIMEOUT),
    Effect.catchAll(() => Effect.succeed(DEFAULT_RESULT)),
    Effect.runPromise,
  );
}

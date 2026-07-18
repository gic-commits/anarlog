import { useCallback, useRef } from "react";

import { commands as analyticsCommands } from "@hypr/plugin-analytics";
<<<<<<< HEAD
=======
import type { TranscriptStorage } from "@hypr/store";
>>>>>>> my-changes

import { useListener } from "./contexts";
import { getSessionKeywords } from "./useKeywords";
import {
  canRunBatchTranscription,
  isStoppedTranscriptionError,
  useRunBatch,
} from "./useRunBatch";
import { useSTTConnection } from "./useSTTConnection";

import { useShell } from "~/contexts/shell";
<<<<<<< HEAD
import {
  deleteProcessedAudioForRetention,
  normalizeAudioRetention,
} from "~/services/audio-retention";
import { getEnhancerService } from "~/services/enhancer";
import { useSession, useSessionHasTranscript } from "~/session/queries";
import { getSessionEvent } from "~/session/utils";
import { useConfigValue } from "~/shared/config";
import { id } from "~/shared/utils";
=======
import { deleteProcessedAudioForRetention } from "~/services/audio-retention";
import { getEnhancerService } from "~/services/enhancer";
import { getSessionEventById } from "~/session/utils";
import { useConfigValue } from "~/shared/config";
import { id } from "~/shared/utils";
import * as main from "~/store/tinybase/store/main";
import * as settings from "~/store/tinybase/store/settings";
>>>>>>> my-changes
import type {
  LiveTranscriptPersistCallback,
  OnStoppedCallback,
} from "~/store/zustand/listener/transcript";
import {
  getLiveTranscriptionConfig,
  getTranscriptionLanguages,
} from "~/stt/capabilities";
import {
<<<<<<< HEAD
  applyLiveTranscriptDeltaToDatabase,
  createLiveTranscript,
  softDeleteTranscript,
  useSessionParticipantHumanIds,
} from "~/stt/queries";
=======
  createTranscriptAccumulator,
  parseTranscriptWords,
  type TranscriptAccumulator,
} from "~/stt/utils";

function hasTranscriptContent(
  store: main.Store,
  indexes: ReturnType<typeof main.UI.useIndexes> | undefined,
  sessionId: string,
) {
  const transcriptIds =
    indexes?.getSliceRowIds(main.INDEXES.transcriptBySession, sessionId) ?? [];

  return transcriptIds.some(
    (transcriptId) => parseTranscriptWords(store, transcriptId).length > 0,
  );
}
>>>>>>> my-changes

export function getPostCaptureAction(
  details: {
    audioPath: string | null;
    liveTranscriptionActive: boolean;
  },
  canRunBatch: boolean,
) {
  if (details.liveTranscriptionActive) {
    return "enhance_only" as const;
  }

  if (!!details.audioPath && canRunBatch) {
    return "batch_then_enhance" as const;
  }

  return "none" as const;
}

export function useStartListening(sessionId: string) {
<<<<<<< HEAD
  const session = useSession(sessionId);
  const hadTranscriptBeforeStart = useSessionHasTranscript(sessionId);
  const participantHumanIds = useSessionParticipantHumanIds(sessionId);
=======
  const { user_id } = main.UI.useValues(main.STORE_ID);
  const store = main.UI.useStore(main.STORE_ID);
  const indexes = main.UI.useIndexes(main.STORE_ID);
  const settingsStore = settings.UI.useStore(settings.STORE_ID);
>>>>>>> my-changes

  const aiLanguage = useConfigValue("ai_language");
  const spokenLanguages = useConfigValue("spoken_languages");
  const dictionaryTerms = useConfigValue("personalization_dictionary_terms");
<<<<<<< HEAD
  const audioRetention = normalizeAudioRetention(
    useConfigValue("audio_retention"),
  );
=======
>>>>>>> my-changes

  const start = useListener((state) => state.start);
  const { conn } = useSTTConnection();
  const runBatch = useRunBatch(sessionId);
  const { leftsidebar } = useShell();
  const setLeftSidebarExpanded = leftsidebar.setExpanded;

  const runBatchRef = useRef(runBatch);
  const canRunBatchRef = useRef(canRunBatchTranscription(conn));
  runBatchRef.current = runBatch;
  canRunBatchRef.current = canRunBatchTranscription(conn);

  const startListening = useCallback(async () => {
<<<<<<< HEAD
    let transcriptId: string | null = null;
    const startedAt = Date.now();
    const memoMd = session?.raw_md ?? "";
    const createdAt = new Date().toISOString();
    let lastTranscriptWrite = Promise.resolve();
    let transcriptWriteError: unknown;
    const trackTranscriptWrite = (write: Promise<void>) => {
      lastTranscriptWrite = write.catch((error) => {
        transcriptWriteError = error;
        console.error("[listener] failed to persist transcript", error);
      });
    };
    const keywords = await getSessionKeywords({
=======
    if (!store) {
      return;
    }

    let transcriptId: string | null = null;
    const startedAt = Date.now();
    const memoMd = store.getCell("sessions", sessionId, "raw_md");
    const createdAt = new Date().toISOString();
    const hadTranscriptBeforeStart = hasTranscriptContent(
      store as main.Store,
      indexes ?? undefined,
      sessionId,
    );
    const transcriptAccumulatorRef: {
      current: TranscriptAccumulator | null;
    } = { current: null };
    const keywords = getSessionKeywords({
      store,
>>>>>>> my-changes
      sessionId,
      dictionaryTerms,
    });

    const onStopped: OnStoppedCallback = async (_sessionId, details) => {
<<<<<<< HEAD
      await lastTranscriptWrite;
      if (transcriptWriteError) return;
=======
      transcriptAccumulatorRef.current?.dispose();
      transcriptAccumulatorRef.current = null;
>>>>>>> my-changes

      const postCaptureAction = getPostCaptureAction(
        details,
        canRunBatchRef.current,
      );

      if (postCaptureAction === "batch_then_enhance") {
        try {
          await runBatchRef.current(details.audioPath!);
        } catch (error) {
          if (isStoppedTranscriptionError(error)) {
            return;
          }
          console.error(
            "[listener] failed to run post-capture transcription",
            error,
          );
          return;
        }
      }

      if (postCaptureAction === "none") {
        return;
      }

      const service = getEnhancerService();
      const shouldRegenerateExistingSummary =
        hadTranscriptBeforeStart &&
        (transcriptId !== null || postCaptureAction === "batch_then_enhance");
      if (shouldRegenerateExistingSummary) {
<<<<<<< HEAD
        await service?.resetEnhanceTasks(sessionId);
        service?.queueAutoEnhance(sessionId);
      } else {
        await service?.queueAutoEnhanceIfSummaryEmpty(sessionId);
      }

      await deleteProcessedAudioForRetention(audioRetention, sessionId);
=======
        service?.resetEnhanceTasks(sessionId);
        service?.queueAutoEnhance(sessionId);
      } else {
        service?.queueAutoEnhanceIfSummaryEmpty(sessionId);
      }

      if (settingsStore) {
        await deleteProcessedAudioForRetention(
          store as main.Store,
          settingsStore as settings.Store,
          sessionId,
        );
      }
>>>>>>> my-changes
    };

    const handlePersist: LiveTranscriptPersistCallback = (delta) => {
      if (delta.new_words.length === 0 && delta.replaced_ids.length === 0) {
        return;
      }

      if (!transcriptId) {
        transcriptId = id();
<<<<<<< HEAD
        trackTranscriptWrite(
          createLiveTranscript(
            {
              id: transcriptId,
              sessionId,
              ownerUserId: session?.user_id ?? "",
              createdAt,
              startedAt,
              memo: memoMd,
              source: "live_capture",
              provider: conn?.provider,
              model: conn?.model,
            },
            delta,
          ),
        );
        return;
      }

      trackTranscriptWrite(
        applyLiveTranscriptDeltaToDatabase(transcriptId, delta),
      );
    };

=======
        const transcriptRow = {
          session_id: sessionId,
          user_id: user_id ?? "",
          created_at: createdAt,
          started_at: startedAt,
          words: "[]",
          speaker_hints: "[]",
          memo_md: typeof memoMd === "string" ? memoMd : "",
        } satisfies TranscriptStorage;

        store.setRow("transcripts", transcriptId, transcriptRow);
        transcriptAccumulatorRef.current = createTranscriptAccumulator(
          store,
          transcriptId,
          { words: [], hints: [] },
        );
      }

      transcriptAccumulatorRef.current ??= createTranscriptAccumulator(
        store,
        transcriptId,
      );

      store.transaction(() => {
        transcriptAccumulatorRef.current?.applyLiveDelta(delta);
      });
    };

    const participantHumanIds: string[] = [];
    store.forEachRow(
      "mapping_session_participant",
      (mappingId, _forEachCell) => {
        const sid = store.getCell(
          "mapping_session_participant",
          mappingId,
          "session_id",
        );
        if (sid !== sessionId) return;
        const hid = store.getCell(
          "mapping_session_participant",
          mappingId,
          "human_id",
        );
        if (typeof hid === "string" && hid) {
          participantHumanIds.push(hid);
        }
      },
    );

>>>>>>> my-changes
    const languages = getTranscriptionLanguages(aiLanguage, spokenLanguages);
    const liveTranscriptionConfig = await getLiveTranscriptionConfig({
      provider: conn?.provider,
      model: conn?.model,
      languages,
    });

    const started = await start(
      {
        session_id: sessionId,
        languages: liveTranscriptionConfig.languages,
        onboarding: false,
        model: conn?.model ?? "",
        base_url: conn?.baseUrl ?? "",
        api_key: conn?.apiKey ?? "",
        keywords,
        transcription_mode: liveTranscriptionConfig.transcriptionMode,
        participant_human_ids: participantHumanIds,
<<<<<<< HEAD
        self_human_id: session?.user_id || null,
=======
        self_human_id: typeof user_id === "string" ? user_id : null,
        provider: conn?.provider,
>>>>>>> my-changes
      },
      {
        handlePersist,
        onStopped,
      },
    );

    if (!started) {
<<<<<<< HEAD
      await lastTranscriptWrite;

      if (transcriptId) {
        await softDeleteTranscript(transcriptId);
=======
      transcriptAccumulatorRef.current?.dispose();
      transcriptAccumulatorRef.current = null;

      if (transcriptId) {
        store.delRow("transcripts", transcriptId);
>>>>>>> my-changes
      }
      return;
    }

    setLeftSidebarExpanded(false);

    void analyticsCommands.event({
      event: "session_started",
<<<<<<< HEAD
      has_calendar_event: Boolean(
        getSessionEvent({ event_json: session?.event_json }),
      ),
=======
      has_calendar_event: !!getSessionEventById(store, sessionId),
>>>>>>> my-changes
      ...(conn
        ? {
            stt_provider: conn.provider,
            stt_model: conn.model,
          }
        : {}),
    });
  }, [
    aiLanguage,
<<<<<<< HEAD
    audioRetention,
    conn,
    dictionaryTerms,
    hadTranscriptBeforeStart,
    participantHumanIds,
    session,
    sessionId,
    start,
    spokenLanguages,
    setLeftSidebarExpanded,
=======
    conn,
    dictionaryTerms,
    store,
    indexes,
    sessionId,
    start,
    user_id,
    spokenLanguages,
    setLeftSidebarExpanded,
    settingsStore,
>>>>>>> my-changes
  ]);

  return startListening;
}

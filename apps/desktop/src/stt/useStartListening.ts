import { useCallback, useMemo, useRef } from "react";

import { commands as analyticsCommands } from "@hypr/plugin-analytics";

import { useListener } from "./contexts";
import { getSessionKeywords } from "./useKeywords";
import {
  canRunBatchTranscription,
  createBatchTranscriptPersist,
  isStoppedTranscriptionError,
  useRunBatch,
} from "./useRunBatch";
import { useSTTConnection } from "./useSTTConnection";

import { useShell } from "~/contexts/shell";
import {
  deleteProcessedAudioForRetention,
  normalizeAudioRetention,
} from "~/services/audio-retention";
import { getEnhancerService } from "~/services/enhancer";
import { useSession, useSessionHasTranscript } from "~/session/queries";
import { getSessionEvent } from "~/session/utils";
import { useConfigValue } from "~/shared/config";
import { id } from "~/shared/utils";
import { waitForLiveBatchResult } from "~/store/zustand/listener/general-batch";
import type {
  LiveTranscriptPersistCallback,
  OnStoppedCallback,
} from "~/store/zustand/listener/transcript";
import {
  getLiveTranscriptionConfig,
  getTranscriptionLanguages,
} from "~/stt/capabilities";
import {
  applyLiveTranscriptDeltaToDatabase,
  createLiveTranscript,
  softDeleteTranscript,
  useSessionParticipantHumanIds,
  useSessionTranscripts,
} from "~/stt/queries";

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
  const session = useSession(sessionId);
  const hadTranscriptBeforeStart = useSessionHasTranscript(sessionId);
  const participantHumanIds = useSessionParticipantHumanIds(sessionId);
  const existingTranscripts = useSessionTranscripts(sessionId);

  // When resuming an existing recording, the incremental batch words use a
  // fresh timeline (starting at 0). Shift them by the latest prior
  // transcript's end time and append instead of replacing. Also offset the
  // incremental speaker indices past the prior max so labels don't collide.
  const existingTranscript = useMemo(() => {
    if (!hadTranscriptBeforeStart) {
      return null;
    }
    const latest = existingTranscripts[existingTranscripts.length - 1];
    if (!latest || latest.words.length === 0) {
      return null;
    }
    const lastWord = latest.words[latest.words.length - 1];
    if (typeof lastWord?.end_ms !== "number") {
      return null;
    }

    let maxSpeakerIndex = -1;
    for (const hint of latest.speakerHints ?? []) {
      if (hint.type !== "provider_speaker_index") {
        continue;
      }
      try {
        const value = JSON.parse(String(hint.value)) as {
          speaker_index?: number;
        };
        if (typeof value.speaker_index === "number") {
          maxSpeakerIndex = Math.max(maxSpeakerIndex, value.speaker_index);
        }
      } catch {
        // ignore malformed hint values
      }
    }

    return {
      transcriptId: latest.id,
      offsetMs: lastWord.end_ms,
      maxSpeakerIndex,
    };
  }, [existingTranscripts, hadTranscriptBeforeStart]);

  const aiLanguage = useConfigValue("ai_language");
  const spokenLanguages = useConfigValue("spoken_languages");
  const dictionaryTerms = useConfigValue("personalization_dictionary_terms");
  const sttMode = useConfigValue("stt_mode");
  const sttSegmentDuration = useConfigValue("stt_segment_duration");
  const diarizationEnabled = useConfigValue("diarization_enabled");
  const diarizationModel = useConfigValue("diarization_model");
  const diarizationThreshold = useConfigValue("diarization_threshold");
  const audioRetention = normalizeAudioRetention(
    useConfigValue("audio_retention"),
  );

  const start = useListener((state) => state.start);
  const selectedMicDevice = useListener(
    (state) => state.live?.selectedMicDevice ?? null,
  );
  const setBatchPersist = useListener((state) => state.setBatchPersist);
  const clearBatchPersist = useListener((state) => state.clearBatchPersist);
  const clearBatchSegments = useListener((state) => state.clearBatchSegments);
  const handleBatchResponse = useListener((state) => state.handleBatchResponse);
  const { conn } = useSTTConnection();
  const runBatch = useRunBatch(sessionId);
  const { leftsidebar } = useShell();
  const setLeftSidebarExpanded = leftsidebar.setExpanded;

  const runBatchRef = useRef(runBatch);
  const canRunBatchRef = useRef(canRunBatchTranscription(conn));
  runBatchRef.current = runBatch;
  canRunBatchRef.current = canRunBatchTranscription(conn);

  const startListening = useCallback(async () => {
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
      sessionId,
      dictionaryTerms,
    });

    const onStopped: OnStoppedCallback = async (_sessionId, details) => {
      await lastTranscriptWrite;
      if (transcriptWriteError) return;

      const postCaptureAction = getPostCaptureAction(
        details,
        canRunBatchRef.current,
      );

      if (isProgressiveBatch) {
        await waitForLiveBatchResult(
          { handleBatchResponse, clearBatchPersist },
          sessionId,
        );
      } else if (postCaptureAction === "batch_then_enhance") {
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
        await service?.resetEnhanceTasks(sessionId);
        service?.queueAutoEnhance(sessionId);
      } else {
        await service?.queueAutoEnhanceIfSummaryEmpty(sessionId);
      }

      await deleteProcessedAudioForRetention(audioRetention, sessionId);
    };

    const handlePersist: LiveTranscriptPersistCallback = (delta) => {
      if (delta.new_words.length === 0 && delta.replaced_ids.length === 0) {
        return;
      }

      if (!transcriptId) {
        transcriptId = id();
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

    const languages = getTranscriptionLanguages(aiLanguage, spokenLanguages);
    const liveTranscriptionConfig = await getLiveTranscriptionConfig({
      provider: conn?.provider,
      model: conn?.model,
      languages,
      sttMode,
    });

    console.log(
      "[DEBUG] useStartListening transcriptionConfig: sttMode=%s transcriptionMode=%s",
      sttMode,
      liveTranscriptionConfig.transcriptionMode,
    );

    const isProgressiveBatch =
      liveTranscriptionConfig.transcriptionMode === "progressiveBatch";

    const batchPersist = isProgressiveBatch
      ? createBatchTranscriptPersist({
          sessionId,
          session,
          provider: conn?.provider ?? "",
          model: conn?.model ?? "",
          existingTranscript,
        })
      : null;

    if (batchPersist) {
      clearBatchSegments(sessionId);
      setBatchPersist(sessionId, batchPersist.persist);
    }

    const startParams = {
      session_id: sessionId,
      languages: liveTranscriptionConfig.languages,
      onboarding: false,
      model: conn?.model ?? "",
      base_url: conn?.baseUrl ?? "",
      api_key: conn?.apiKey ?? "",
      keywords,
      transcription_mode: liveTranscriptionConfig.transcriptionMode,
      provider: conn?.provider ?? null,
      participant_human_ids: participantHumanIds,
      self_human_id: session?.user_id || null,
      segment_duration_ms: sttSegmentDuration ?? undefined,
      diarization_enabled: diarizationEnabled,
      diarization_model: diarizationModel,
      diarization_threshold: diarizationThreshold,
      mic_device: selectedMicDevice,
    };

    console.log(
      "[DEBUG] useStartListening: conn=%o startParams=%o",
      conn,
      startParams,
    );

    const started = await start(startParams, {
      handlePersist,
      onStopped,
    });

    if (!started) {
      await lastTranscriptWrite;

      if (transcriptId) {
        await softDeleteTranscript(transcriptId);
      }

      if (batchPersist) {
        clearBatchPersist(sessionId);
      }
      return;
    }

    setLeftSidebarExpanded(false);

    void analyticsCommands.event({
      event: "session_started",
      has_calendar_event: Boolean(
        getSessionEvent({ event_json: session?.event_json }),
      ),
      ...(conn
        ? {
            stt_provider: conn.provider,
            stt_model: conn.model,
          }
        : {}),
    });
  }, [
    aiLanguage,
    audioRetention,
    clearBatchPersist,
    clearBatchSegments,
    conn,
    dictionaryTerms,
    hadTranscriptBeforeStart,
    handleBatchResponse,
    participantHumanIds,
    session,
    sessionId,
    setBatchPersist,
    start,
    spokenLanguages,
    sttMode,
    sttSegmentDuration,
    diarizationEnabled,
    diarizationModel,
    diarizationThreshold,
    selectedMicDevice,
    existingTranscript,
    setLeftSidebarExpanded,
  ]);

  return startListening;
}

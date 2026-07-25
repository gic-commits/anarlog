import type { RefObject } from "react";
import { useCallback, useMemo } from "react";

import { Spinner } from "@hypr/ui/components/ui/spinner";

import { useRegenerateTranscript } from "./actions";
import { TranscriptViewer } from "./renderer";
import { BatchState } from "./screens/batch";
import { TranscriptEmptyState } from "./screens/empty";
import { TranscriptListeningState } from "./screens/listening";
import { useTranscriptScreen } from "./state";

import { useListener } from "~/stt/contexts";
import { useUploadFile } from "~/stt/useUploadFile";

export function Transcript({
  sessionId,
  scrollRef,
}: {
  sessionId: string;
  scrollRef: RefObject<HTMLDivElement | null>;
}) {
  const screen = useTranscriptScreen({ sessionId });
  const { uploadAudio, uploadTranscript } = useUploadFile(sessionId);
  const regenerateTranscript = useRegenerateTranscript(sessionId);
  const stopTranscription = useListener((state) => state.stopTranscription);
  const handleStopTranscription = useCallback(() => {
    void stopTranscription(sessionId);
  }, [sessionId, stopTranscription]);

  return (
    <div className="relative flex h-full flex-col overflow-hidden">
      {screen.kind === "running_batch" && (
        <>
          <TranscriptEmptyState
            isBatching
            percentage={screen.percentage}
            phase={screen.phase}
            segmentCount={Object.keys(screen.segmentResponses).length}
            onStopTranscription={
              screen.phase === "importing" ? undefined : handleStopTranscription
            }
          />
          <SegmentPreview segmentResponses={screen.segmentResponses} />
        </>
      )}
      {screen.kind === "batch_fallback" && (
        <BatchState
          requestedLiveTranscription={screen.requestedLiveTranscription}
          error={screen.error}
        />
      )}
      {screen.kind === "listening" && (
        <TranscriptListeningState status={screen.status} />
      )}
      {screen.kind === "empty" && (
        <TranscriptEmptyState
          isBatching={false}
          hasAudio={screen.hasAudio}
          error={screen.error}
          onRetranscribe={regenerateTranscript}
          onUploadAudio={uploadAudio}
          onUploadTranscript={uploadTranscript}
        />
      )}
      {screen.kind === "ready" && (
        <>
          {screen.isFinalizing ? <FinalizingTranscriptBanner /> : null}
          <TranscriptViewer
            transcriptIds={screen.transcriptIds}
            liveSegments={screen.liveSegments}
            currentActive={screen.currentActive}
            scrollRef={scrollRef}
          />
        </>
      )}
    </div>
  );
}

function SegmentPreview({
  segmentResponses,
}: {
  segmentResponses: Record<
    number,
    import("@hypr/plugin-transcription").BatchResponse
  >;
}) {
  const entries = useMemo(
    () =>
      Object.entries(segmentResponses)
        .map(([index, response]) => ({
          index: Number(index),
          transcript:
            response.results.channels[0]?.alternatives[0]?.transcript ?? "",
        }))
        .sort((a, b) => a.index - b.index),
    [segmentResponses],
  );

  if (entries.length === 0) {
    return null;
  }

  return (
    <div className="scrollbar-thin flex-1 space-y-3 overflow-y-auto px-4 pb-4">
      {entries.map(({ index, transcript }, i) => (
        <div key={index}>
          {i > 0 && (
            <div className="border-t-border mx-2 my-3 border-t border-dashed" />
          )}
          <div className="group relative">
            <div className="text-muted-foreground mb-1 text-xs font-medium">
              Segment {index + 1}
            </div>
            <p className="text-foreground text-sm leading-relaxed whitespace-pre-wrap">
              {transcript || "(no speech detected)"}
            </p>
          </div>
        </div>
      ))}
    </div>
  );
}

function FinalizingTranscriptBanner() {
  return (
    <div className="bg-background/95 pointer-events-none absolute top-3 left-1/2 z-10 flex -translate-x-1/2 items-center gap-2 rounded-full border px-3 py-1.5 text-xs font-medium shadow-sm">
      <Spinner size={14} />
      <span>Finalizing transcript...</span>
    </div>
  );
}

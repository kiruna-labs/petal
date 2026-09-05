import assert from 'node:assert/strict';
import test from 'node:test';

import { attachVideoStream } from '../src/lib/videoAttachment.ts';

type TileVideoState = {
  muted: boolean;
  sharing: boolean;
  videoStream?: MediaStream;
};

function mediaStream(id: string): MediaStream {
  return { id } as unknown as MediaStream;
}

function videoSpy(initialSrcObject: MediaStream | null = null) {
  let srcObject = initialSrcObject;
  const assignments: Array<MediaStream | null> = [];
  const video = {
    get srcObject() {
      return srcObject;
    },
    set srcObject(value: MediaStream | null) {
      assignments.push(value);
      srcObject = value;
    }
  } as unknown as HTMLVideoElement;

  return { video, assignments };
}

function attachParticipantVideo(video: HTMLVideoElement, participant: TileVideoState): boolean {
  return attachVideoStream(video, participant.videoStream);
}

test('attachVideoStream is a no-op when the stream is unchanged', () => {
  const stream = { id: 'camera-1' } as unknown as MediaStream;
  const { video, assignments } = videoSpy(stream);

  assert.equal(attachVideoStream(video, stream), false);
  assert.equal(video.srcObject, stream);
  assert.deepEqual(assignments, []);
});

test('attachVideoStream assigns only when the normalized stream changes', () => {
  const first = mediaStream('camera-1');
  const second = mediaStream('camera-2');
  const { video, assignments } = videoSpy(first);

  assert.equal(attachVideoStream(video, second), true);
  assert.equal(video.srcObject, second);
  assert.deepEqual(assignments, [second]);

  assert.equal(attachVideoStream(video, undefined), true);
  assert.equal(video.srcObject, null);
  assert.deepEqual(assignments, [second, null]);

  assert.equal(attachVideoStream(video, null), false);
  assert.equal(video.srcObject, null);
  assert.deepEqual(assignments, [second, null]);
});

test('mute/share-only participant churn does not reassign an unchanged webcam stream', () => {
  const stream = mediaStream('camera-1');
  const { video, assignments } = videoSpy();
  const participantArrays: TileVideoState[][] = [
    [{ muted: false, sharing: false, videoStream: stream }],
    [{ muted: true, sharing: false, videoStream: stream }],
    [{ muted: true, sharing: true, videoStream: stream }]
  ];

  assert.equal(attachParticipantVideo(video, participantArrays[0][0]), true);
  assert.equal(video.srcObject, stream);
  assert.deepEqual(assignments, [stream]);

  assert.equal(attachParticipantVideo(video, participantArrays[1][0]), false);
  assert.equal(attachParticipantVideo(video, participantArrays[2][0]), false);
  assert.equal(video.srcObject, stream);
  assert.deepEqual(assignments, [stream]);
});

test('participant video churn still reassigns when the stream reference changes', () => {
  const first = mediaStream('camera-1');
  const second = mediaStream('camera-2');
  const { video, assignments } = videoSpy();

  assert.equal(
    attachParticipantVideo(video, { muted: false, sharing: false, videoStream: first }),
    true
  );
  assert.equal(
    attachParticipantVideo(video, { muted: true, sharing: true, videoStream: second }),
    true
  );

  assert.equal(video.srcObject, second);
  assert.deepEqual(assignments, [first, second]);
});

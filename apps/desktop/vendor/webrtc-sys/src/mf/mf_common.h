// Shared Media Foundation bootstrap for the Windows H.264 hardware codec
// path. Modeled on Chromium's MFVEA/MFVDA architecture (media/gpu/windows/):
// a hardware MFT is found via MFTEnumEx, activated, and — if it is an
// asynchronous MFT — unlocked with MF_TRANSFORM_ASYNC_UNLOCK. The encoder is
// then driven by the generic async contract (IMFMediaEventGenerator events),
// not by any vendor-specific workaround.

#pragma once

#include <atomic>
#include <memory>
#include <vector>

#include <windows.h>
#include <codecapi.h>
#include <mfapi.h>
#include <mferror.h>
#include <mfidl.h>
#include <mftransform.h>
#include <wrl/client.h>

#if defined(_MSC_VER)
#pragma comment(lib, "mfplat")
#pragma comment(lib, "mf")
#pragma comment(lib, "mfuuid")
#endif

namespace webrtc {

// GUID constants (from mfapi.h / mftransform.h, spelled out for clarity).
namespace mf_guids {
inline const GUID kH264 = {0x34363248, 0x0000, 0x0010, {0x80, 0x00, 0x00, 0xAA,
                                                         0x00, 0x38, 0x9B, 0x71}};  // 'H264'
inline const GUID kNv12 = {0x3231564E, 0x0000, 0x0010, {0x80, 0x00, 0x00, 0xAA,
                                                        0x00, 0x38, 0x9B, 0x71}};  // 'NV12'
inline const GUID kCategoryVideoEncoder = {
    0xF79EAC7D, 0xE545, 0x4387, {0xBD, 0xEE, 0xD6, 0x47, 0xD7, 0xBD, 0xE4, 0x2A}};
inline const GUID kCategoryVideoDecoder = {
    0xD6C02D4B, 0x6833, 0x45B4, {0x97, 0x1A, 0x05, 0xA4, 0xB0, 0x4B, 0xAB, 0x91}};
}  // namespace mf_guids

// CoInitializeEx + MFStartup once per process. Safe to call repeatedly.
inline bool EnsureMediaFoundationStarted() {
  static std::atomic<bool> initialized = false;
  if (initialized.load(std::memory_order_acquire)) {
    return true;
  }
  HRESULT hr = CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);
  // RPC_E_CHANGED_MODE means COM was already initialized on this thread with
  // a different model; that is fine for our usage.
  if (FAILED(hr) && hr != RPC_E_CHANGED_MODE) {
    return false;
  }
  hr = MFStartup(MF_VERSION, MFSTARTUP_LITE);
  if (FAILED(hr)) {
    return false;
  }
  initialized.store(true, std::memory_order_release);
  return true;
}

// Ensure COM is initialized on the calling thread (required on every thread
// that touches Media Foundation objects). Balanced by a thread_local guard so
// repeated calls do not unbalance the COM refcount.
inline void EnsureComInitialized() {
  thread_local bool com_ready = false;
  if (com_ready) {
    return;
  }
  HRESULT hr = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
  com_ready = SUCCEEDED(hr) || hr == RPC_E_CHANGED_MODE || hr == S_FALSE;
}

// Query the MFT's global attributes and, if it is an asynchronous MFT,
// unlock it with MF_TRANSFORM_ASYNC_UNLOCK so ProcessInput/ProcessOutput may
// be called directly instead of through the internal async work queue.
// Returns S_OK on success (also for synchronous MFTs, which need no unlock).
inline HRESULT UnlockAsyncTransform(IMFTransform* transform) {
  if (!transform) {
    return E_POINTER;
  }
  Microsoft::WRL::ComPtr<IMFAttributes> attributes;
  HRESULT hr = transform->GetAttributes(&attributes);
  if (FAILED(hr)) {
    return hr;
  }
  UINT32 async = FALSE;
  hr = attributes->GetUINT32(MF_TRANSFORM_ASYNC, &async);
  if (FAILED(hr)) {
    // Synchronous MFT: nothing to unlock.
    return S_OK;
  }
  if (!async) {
    return S_OK;
  }
  return attributes->SetUINT32(MF_TRANSFORM_ASYNC_UNLOCK, TRUE);
}

// Returns true if `transform` advertises H264 among its output subtypes
// (is_output=true) or input subtypes (is_output=false).
inline bool MftAcceptsH264Subtype(IMFTransform* transform, bool is_output) {
  for (DWORD i = 0; i < 32; ++i) {
    Microsoft::WRL::ComPtr<IMFMediaType> type;
    HRESULT hr = is_output ? transform->GetOutputAvailableType(0, i, &type)
                           : transform->GetInputAvailableType(0, i, &type);
    if (hr == MF_E_NO_MORE_TYPES) {
      break;
    }
    if (FAILED(hr)) {
      continue;
    }
    GUID subtype = {0};
    if (SUCCEEDED(type->GetGUID(MF_MT_SUBTYPE, &subtype))) {
      if (subtype == mf_guids::kH264) {
        return true;
      }
    }
  }
  return false;
}


// Enumerate MFTs of the given category, optionally restricted to hardware
// (MFT_ENUM_FLAG_HARDWARE). Returns the first activated MFT whose output
// subtypes include H264 (for encoders) / input subtypes include H264 (for
// decoders). `match_clsid` may be set to a specific MFT CLSID (as a debug
// hint / for determinism); when null the first matching MFT is used.
inline HRESULT FindAndActivateH264Mft(
    const GUID& category,
    bool hardware_only,
    const GUID* match_clsid,
    IMFTransform** out_transform) {
  if (!out_transform) {
    return E_POINTER;
  }
  *out_transform = nullptr;
  if (!EnsureMediaFoundationStarted()) {
    return E_FAIL;
  }

  // Registry type filters narrow the enumeration to H264 MFTs without
  // activating anything (an async MFT that is still locked refuses type
  // queries with MF_E_TRANSFORM_ASYNC_LOCKED, so the subtype scan below can
  // only run AFTER unlock — the filters are the primary match).
  const bool is_encoder = (category == mf_guids::kCategoryVideoEncoder);
  MFT_REGISTER_TYPE_INFO input_type = {
      MFMediaType_Video,
      is_encoder ? mf_guids::kNv12 : mf_guids::kH264};
  MFT_REGISTER_TYPE_INFO output_type = {
      MFMediaType_Video,
      is_encoder ? mf_guids::kH264 : mf_guids::kNv12};

  UINT32 flags = MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_ASYNCMFT |
                 MFT_ENUM_FLAG_HARDWARE;
  if (!hardware_only) {
    flags |= MFT_ENUM_FLAG_SORTANDFILTER;
  }

  Microsoft::WRL::ComPtr<IMFTransform> chosen;
  {
    IMFActivate** activates = nullptr;
    UINT32 count = 0;
    HRESULT hr = MFTEnumEx(category, flags, &input_type, &output_type,
                           &activates, &count);
    if (FAILED(hr)) {
      return hr;
    }
    for (UINT32 i = 0; i < count; ++i) {
      GUID clsid = {0};
      if (SUCCEEDED(activates[i]->GetGUID(MFT_TRANSFORM_CLSID_Attribute,
                                          &clsid))) {
        if (match_clsid && *match_clsid != clsid) {
          continue;
        }
        Microsoft::WRL::ComPtr<IMFTransform> candidate;
        hr = activates[i]->ActivateObject(IID_PPV_ARGS(&candidate));
        if (FAILED(hr)) {
          continue;
        }
        // Sanity check that the MFT actually produces/consumes H264. Must
        // unlock async MFTs first — a locked MFT answers type queries with
        // MF_E_TRANSFORM_ASYNC_LOCKED.
        UnlockAsyncTransform(candidate.Get());
        bool has_h264 = MftAcceptsH264Subtype(candidate.Get(), is_encoder);
        if (!has_h264) {
          activates[i]->ShutdownObject();
          continue;
        }
        chosen = candidate;
        break;
      }
    }
    for (UINT32 i = 0; i < count; ++i) {
      activates[i]->Release();
    }
    CoTaskMemFree(activates);
  }
  if (!chosen) {
    return MF_E_NOT_FOUND;
  }
  // `out_transform` is a raw IMFTransform** (callers pass &mft_ via
  // ComPtr::operator&), so this writes the raw pointer straight into the
  // ComPtr storage WITHOUT AddRef: the ActivateObject reference is
  // transferred, not leaked.
  *out_transform = chosen.Detach();
  return S_OK;
}


// Resolve stream IDs. Returns {input_id, output_id}, falling back to {0, 0}
// when the MFT reports E_NOTIMPL.
inline bool ResolveStreamIds(IMFTransform* transform, DWORD* input_id,
                             DWORD* output_id) {
  DWORD input_count = 0;
  DWORD output_count = 0;
  if (FAILED(transform->GetStreamCount(&input_count, &output_count)) ||
      input_count < 1 || output_count < 1) {
    return false;
  }
  std::vector<DWORD> input_ids(input_count, 0);
  std::vector<DWORD> output_ids(output_count, 0);
  HRESULT hr = transform->GetStreamIDs(input_count, input_ids.data(),
                                       output_count, output_ids.data());
  if (SUCCEEDED(hr)) {
    *input_id = input_ids[0];
    *output_id = output_ids[0];
  } else if (hr == E_NOTIMPL) {
    *input_id = 0;
    *output_id = 0;
  } else {
    return false;
  }
  return true;
}

}  // namespace webrtc

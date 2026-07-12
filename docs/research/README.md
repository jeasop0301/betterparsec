# 리서치 · 의사결정 기록 (process)

betterparsec의 방향은 3차에 걸친 다중 에이전트 리서치 + 적대적 검증으로 결정됐다. 각 문서는 종합 메모 + 검증 판정(CONFIRMED/DISPUTED/CONTEXT-DEPENDENT/UNVERIFIABLE)을 담는다.

1. [01 — Parsec vs Moonlight/Sunshine, 브라우저 무설치 옵션](01-parsec-vs-open-alternatives.md)
   → 결론: Parsec은 포크 불가(closed). "Moonlight가 낫다"는 조건부. Parsec Web App은 무설치 존재.
2. [02 — 저비트율 ‘뭉개짐’ 심층 진단](02-lowbitrate-mushing-deepdive.md)
   → 결론: 뭉개짐 주범은 코덱이 아니라 WARP 전송 + 적응형 비트레이트 부재. AV1은 ~한 눈금.
3. [03 — 실제 만들 수 있는 설계 검증](03-buildable-design.md)
   → 결론: 재페어링은 코드 없이 해결됨. 진짜 병목은 연결성+적응성. 정직한 build-vs-buy.

시각 요약: [../design-memo.html](../design-memo.html) (브라우저로 열기).

핵심 정리와 seam은 [../ARCHITECTURE.md](../ARCHITECTURE.md), 실행 계획은 [../ROADMAP.md](../ROADMAP.md), 인수인계는 [../../HANDOFF.md](../../HANDOFF.md).

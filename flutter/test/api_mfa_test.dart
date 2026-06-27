// CE-M1-4: API 账号 MFA 数据契约单测。
//
// 这里聚焦在 LoginResponse / LoginRequest 数据层的兼容性,因为 UI 路径(apiMfaDialog
// / handleLoginResponse)依赖 gFFI/native bind 与 dialog manager,在 pure dart unit
// test 进程里没法复现。完整的 UI 流程在 §6 表格中保留,留给 CE-M1-10 的端到端回归。
//
// 这里覆盖的信号:
// 1. 老服务端响应(无 mfa_required 字段)解析后 mfaRequired==false,行为不变。
// 2. 新服务端响应(mfa_required:true + ticket + mfa_methods)能被正确读出。
// 3. LoginResponse.toString 永远不暴露 mfa_ticket 明文,确保日志卫生。

import 'package:flutter_test/flutter_test.dart';
import 'package:flutter_hbb/common/hbbs/hbbs.dart';

void main() {
  group('LoginResponse.fromJson', () {
    test('legacy access_token response keeps mfaRequired=false', () {
      final resp = LoginResponse.fromJson({
        'access_token': 'abc',
        'type': 'access_token',
        'user': {
          'name': 'alice',
          'display_name': 'Alice',
          'email': 'alice@example.com',
          'status': 1,
          'is_admin': false,
        },
      });
      expect(resp.access_token, 'abc');
      expect(resp.type, HttpType.kAuthResTypeToken);
      expect(resp.mfaRequired, isFalse);
      expect(resp.mfaTicket, isNull);
      expect(resp.mfaMethods, isNull);
      expect(resp.user?.name, 'alice');
    });

    test('legacy email_check response keeps original tfa_type fields', () {
      final resp = LoginResponse.fromJson({
        'type': 'email_check',
        'tfa_type': 'tfa_check',
        'secret': 's',
        'user': {'name': 'x', 'email': 'x@example.com', 'status': 1},
      });
      expect(resp.type, HttpType.kAuthResTypeEmailCheck);
      expect(resp.tfa_type, HttpType.kAuthResTypeTfaCheck);
      expect(resp.secret, 's');
      // 关键:老路径绝不能被错误识别为 API 账号 MFA。
      expect(resp.mfaRequired, isFalse);
      expect(resp.mfaTicket, isNull);
    });

    test('new mfa_required response is parsed', () {
      final resp = LoginResponse.fromJson({
        'type': HttpType.kAuthResTypeMfaRequired,
        'mfa_required': true,
        'ticket': 'T1',
        'mfa_methods': ['totp', 'recovery_code'],
      });
      expect(resp.type, HttpType.kAuthResTypeMfaRequired);
      expect(resp.mfaRequired, isTrue);
      expect(resp.mfaTicket, 'T1');
      expect(resp.mfaMethods, ['totp', 'recovery_code']);
      expect(resp.access_token, isNull);
    });

    test('mfa_required without ticket sets ticket=null', () {
      final resp = LoginResponse.fromJson({
        'type': HttpType.kAuthResTypeMfaRequired,
        'mfa_required': true,
      });
      expect(resp.mfaRequired, isTrue);
      expect(resp.mfaTicket, isNull);
      // handleLoginResponse 在这种情况下应回退到错误提示,而非弹出对话框。
    });

    test('mfa_methods of non-list shape stays null', () {
      final resp = LoginResponse.fromJson({
        'type': HttpType.kAuthResTypeMfaRequired,
        'mfa_required': true,
        'ticket': 'T',
        'mfa_methods': 'totp',
      });
      expect(resp.mfaMethods, isNull);
    });

    test('toString never leaks the ticket value', () {
      final resp = LoginResponse.fromJson({
        'type': HttpType.kAuthResTypeMfaRequired,
        'mfa_required': true,
        'ticket': 'super-secret-jwt-not-to-be-logged',
      });
      final str = resp.toString();
      expect(str.contains('super-secret-jwt-not-to-be-logged'), isFalse,
          reason: 'LoginResponse.toString must not log raw mfa ticket');
      expect(str.contains('hasMfaTicket: true'), isTrue);
    });
  });

  group('HttpType / MfaMethod constants', () {
    test('new auth constants present', () {
      expect(HttpType.kAuthResTypeMfaRequired, 'mfa_required');
      expect(HttpType.kAuthReqTypeMfaCode, 'mfa_code');
    });

    test('mfa method constants present', () {
      expect(MfaMethod.kTotp, 'totp');
      expect(MfaMethod.kRecoveryCode, 'recovery_code');
    });
  });
}

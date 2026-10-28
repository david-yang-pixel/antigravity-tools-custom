//! SMTP delivery for usage-portal verification codes. Messages are plain text and
//! bilingual (Chinese first, then English), matching the portal's two languages.

use crate::modules::portal_account::{CodePurpose, CODE_TTL_SECS};
use crate::modules::portal_settings::{SmtpSecurity, SmtpSettings};
use lettre::message::{header::ContentType, Mailbox};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use std::time::Duration;

pub struct Mail {
    pub subject: String,
    pub body: String,
}

pub fn brand(smtp: &SmtpSettings) -> &str {
    if smtp.from_name.is_empty() {
        "API Usage Portal"
    } else {
        &smtp.from_name
    }
}

pub fn code_mail(smtp: &SmtpSettings, purpose: CodePurpose, code: &str) -> Mail {
    let minutes = CODE_TTL_SECS / 60;
    let (zh, en) = match purpose {
        CodePurpose::Register => ("注册验证码", "sign-up code"),
        CodePurpose::Reset => ("重置密码验证码", "password reset code"),
    };
    Mail {
        subject: format!("[{}] {zh} / {en}: {code}", brand(smtp)),
        body: format!(
            "你的{zh}是：{code}\n验证码 {minutes} 分钟内有效。如果不是你本人操作，请忽略这封邮件。\n\n\
             Your {en} is: {code}\nIt expires in {minutes} minutes. If you did not request it, ignore this email.\n"
        ),
    }
}

/// Sent instead of a code when someone tries to register an existing address.
pub fn already_registered_mail(smtp: &SmtpSettings) -> Mail {
    Mail {
        subject: format!("[{}] 账号已存在 / Account already exists", brand(smtp)),
        body: "有人使用这个邮箱申请注册，但这个邮箱已经有账号了。如果是你本人，请直接登录；忘记密码可以在登录页使用「忘记密码」找回。如果不是你本人操作，请忽略这封邮件。\n\n\
               Someone tried to sign up with this email, but it already has an account. If that was you, sign in, or use \"Forgot password\" on the sign-in page. Otherwise, ignore this email.\n"
            .into(),
    }
}

pub fn test_mail(smtp: &SmtpSettings) -> Mail {
    Mail {
        subject: format!("[{}] 测试邮件 / Test email", brand(smtp)),
        body: "SMTP 配置可用，用量中心可以发送验证码邮件。\n\nSMTP is working; the usage portal can send verification emails.\n".into(),
    }
}

pub async fn send(smtp: &SmtpSettings, to: &str, mail: Mail) -> Result<(), String> {
    if !smtp.is_ready() {
        return Err("SMTP is not configured".into());
    }
    let from = Mailbox::new(
        (!smtp.from_name.is_empty()).then(|| smtp.from_name.clone()),
        smtp.from_address
            .parse()
            .map_err(|e| format!("Invalid sender address: {e}"))?,
    );
    let to: Mailbox = to
        .parse()
        .map_err(|e| format!("Invalid recipient address: {e}"))?;
    let message = Message::builder()
        .from(from)
        .to(to)
        .subject(mail.subject)
        .header(ContentType::TEXT_PLAIN)
        .body(mail.body)
        .map_err(|e| format!("Failed to build email: {e}"))?;

    let builder = match smtp.security {
        SmtpSecurity::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(&smtp.host),
        SmtpSecurity::Starttls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&smtp.host),
        SmtpSecurity::None => Ok(AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(
            &smtp.host,
        )),
    }
    .map_err(|e| format!("Invalid SMTP host: {e}"))?;
    let mut builder = builder
        .port(smtp.port)
        .timeout(Some(Duration::from_secs(20)));
    if !smtp.username.is_empty() {
        builder = builder.credentials(Credentials::new(
            smtp.username.clone(),
            smtp.password.clone(),
        ));
    }
    builder
        .build()
        .send(message)
        .await
        .map(|_| ())
        .map_err(|e| format!("SMTP delivery failed: {e}"))
}

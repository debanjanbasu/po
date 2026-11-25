//! Chat feature: contacts list and conversation views.

mod add_contact;
mod chat_list;
mod conversation_view;

pub use chat_list::ChatList as Chat;
pub use conversation_view::ConversationView as ChatConversation;

/*!
 The main app runtime.
*/

use std::{
    cmp::min,
    collections::{BTreeSet, HashMap, HashSet},
    fs::{self, File, OpenOptions, create_dir_all, remove_dir_all, remove_file, rename},
    io::Write,
    path::PathBuf,
    thread,
    time::Duration,
};

use crabapple::Backup;
use rusqlite::Connection;

use crate::{
    TXT,
    app::{
        compatibility::backup::{decrypt_backup, get_decrypted_message_database},
        error::RuntimeError,
        options::{OPTION_CLEARTEXT_PASSWORD, Options},
        sanitizers::sanitize_filename,
    },
    exporters::exporter::ATTACHMENT_NO_FILENAME,
};

use imessage_database::{
    tables::{
        attachment::Attachment,
        chat::Chat,
        chat_handle::ChatToHandle,
        handle::Handle,
        messages::Message,
        table::{
            ATTACHMENTS_DIR, Cacheable, Deduplicate, ME, ORPHANED, UNKNOWN,
            get_connection,
        },
    },
    util::{dates::get_offset, platform::Platform},
};

const MAX_LENGTH: usize = 235;

/// Stores the application state and handles application lifecycle
pub struct Config {
    /// Map of chatroom ID to chatroom information
    pub chatrooms: HashMap<i32, Chat>,
    /// Map of chatroom ID to an internal unique chatroom ID
    pub real_chatrooms: HashMap<i32, i32>,
    /// Map of chatroom ID to chatroom participants
    pub chatroom_participants: HashMap<i32, BTreeSet<i32>>,
    /// Map of participant ID to contact info
    pub participants: HashMap<i32, String>,
    /// Map of participant ID to an internal unique participant ID
    pub real_participants: HashMap<i32, i32>,
    /// Messages that are tapbacks (reactions) to other messages
    pub tapbacks: HashMap<String, HashMap<usize, Vec<Message>>>,
    /// App configuration options
    pub options: Options,
    /// Global date offset used by the iMessage database:
    pub offset: i64,
    /// The connection we use to query the database
    pub db: Option<Connection>,
    /// An optional encrypted iOS backup
    pub backup: Option<Backup>,
}

impl Config {
    /// Get a deduplicated chat ID or a default value
    pub fn conversation(&self, message: &Message) -> Option<(&Chat, &i32)> {
        match message.chat_id.or(message.deleted_from) {
            Some(chat_id) => {
                if let Some(chatroom) = self.chatrooms.get(&chat_id) {
                    self.real_chatrooms.get(&chat_id).map(|id| (chatroom, id))
                } else {
                    eprintln!("Chat ID {chat_id} does not exist in chat table!");
                    None
                }
            }
            // No chat_id provided
            None => None,
        }
    }

    /// Get the attachment path for the current session
    pub fn attachment_path(&self) -> PathBuf {
        let mut path = self.options.export_path.clone();
        path.push(ATTACHMENTS_DIR);
        path
    }

    pub fn tmp_attachment_path(&self) -> PathBuf {
        let mut path = self.attachment_path();
        path.push("tmp");
        path
    }

    /// Get the attachment path for a specific chat ID
    pub fn conversation_attachment_path(&self, chat_id: Option<i32>) -> String {
        if let Some(chat_id) = chat_id {
            if let Some(real_id) = self.real_chatrooms.get(&chat_id) {
                return real_id.to_string();
            }
        }
        String::from(ORPHANED)
    }

    /// Generate a file path for an attachment
    ///
    /// If the attachment was copied, use that path
    /// if not, default to the filename
    pub fn message_attachment_path(&self, attachment: &Attachment) -> String {
        // Build a relative filepath from the fully qualified one on the `Attachment`
        match &attachment.copied_path {
            Some(path) => {
                if let Ok(relative_path) = path.strip_prefix(&self.options.export_path) {
                    return relative_path.display().to_string();
                }
                path.display().to_string()
            }
            None => attachment
                .resolved_attachment_path(
                    &self.options.platform,
                    &self.options.db_path,
                    self.options.attachment_root.as_deref(),
                )
                .unwrap_or_else(|| {
                    attachment
                        .filename()
                        .unwrap_or(ATTACHMENT_NO_FILENAME)
                        .to_string()
                }),
        }
    }

    /// Get a relative path for the provided file.
    pub fn relative_path(&self, path: PathBuf) -> Option<String> {
        if let Ok(relative_path) = path.strip_prefix(&self.options.export_path) {
            return Some(relative_path.display().to_string());
        }
        Some(path.display().to_string())
    }

    /// Get a filename for a chat, possibly using cached data.
    ///
    /// If the chat has an assigned name, use that, truncating if necessary.
    ///
    /// If it does not, first try and make a flat list of its members. Failing that, use the unique `chat_identifier` field.
    pub fn filename(&self, chatroom: &Chat) -> String {
        let filename = match &chatroom.display_name() {
            // If there is a display name, use that
            Some(name) => {
                format!(
                    "{} - {}",
                    &name[..min(MAX_LENGTH, name.len())],
                    chatroom.rowid
                )
            }
            // Fallback if there is no name set
            None => {
                if let Some(participants) = self.chatroom_participants.get(&chatroom.rowid) {
                    self.filename_from_participants(participants)
                } else {
                    eprintln!(
                        "Found error: message chat ID {} has no members!",
                        chatroom.rowid
                    );
                    chatroom.chat_identifier.clone()
                }
            }
        };

        sanitize_filename(&filename)
    }

    /// Generate a filename from a set of participants, truncating if the name is too long
    ///
    /// - All names:
    ///   - Contact 1, Contact 2
    /// - Truncated Names
    ///   - Contact 1, Contact 2, ... Contact 13 and 4 others
    fn filename_from_participants(&self, participants: &BTreeSet<i32>) -> String {
        let mut added = 0;
        let mut out_s = String::with_capacity(MAX_LENGTH);
        for participant_id in participants {
            let participant = self.who(Some(*participant_id), false, &None);
            if participant.len() + out_s.len() < MAX_LENGTH {
                if !out_s.is_empty() {
                    out_s.push_str(", ");
                }
                out_s.push_str(participant);
                added += 1;
            } else {
                let extra = format!(", and {} others", participants.len() - added);
                let space_remaining = extra.len() + out_s.len();
                if space_remaining >= MAX_LENGTH {
                    out_s.replace_range((MAX_LENGTH - extra.len()).., &extra);
                } else if out_s.is_empty() {
                    out_s.push_str(&participant[..MAX_LENGTH]);
                } else {
                    out_s.push_str(&extra);
                }
                break;
            }
        }
        out_s
    }

    /// Create a new instance of the application
    ///
    /// # Example:
    ///
    /// ```
    /// use crate::app::{
    ///    options::{from_command_line, Options},
    ///    runtime::Config,
    /// };
    ///
    /// let args = from_command_line();
    /// let options = Options::from_args(&args);
    /// let app = Config::new(options).unwrap();
    /// ```
    pub fn new(options: Options) -> Result<Config, RuntimeError> {
        let backup = decrypt_backup(&options)?;
        let conn = match &backup {
            Some(b) => get_connection(&get_decrypted_message_database(b)?),
            None => get_connection(&options.get_db_path()),
        }?;

        // Check if the backup is encrypted and a password was not provided
        if matches!(options.platform, Platform::iOS)
            && backup.is_none()
            && conn.query_row("SELECT 1", [], |_| Ok(())).is_err()
        {
            return Err(RuntimeError::InvalidOptions(format!(
                "The provided iOS backup is encrypted, but no password was provided. Please provide a password using the --{OPTION_CLEARTEXT_PASSWORD} option."
            )));
        }

        eprintln!("Building cache...");
        eprintln!("  [1/4] Caching chats...");
        let chatrooms = Chat::cache(&conn)?;
        eprintln!("  [2/4] Caching chatrooms...");
        let chatroom_participants = ChatToHandle::cache(&conn)?;
        eprintln!("  [3/4] Caching participants...");
        let participants = Handle::cache(&conn)?;
        eprintln!("  [4/4] Caching tapbacks...");
        let tapbacks = Message::cache(&conn)?;
        eprintln!("Cache built!");

        Ok(Config {
            chatrooms,
            real_chatrooms: ChatToHandle::dedupe(&chatroom_participants),
            chatroom_participants,
            real_participants: Handle::dedupe(&participants),
            participants,
            tapbacks,
            options,
            offset: get_offset(),
            db: Some(conn),
            backup,
        })
    }

    /// Get the current database connection, if it is alive
    ///
    /// # Panics
    ///
    /// Panics if the database connection is closed.
    pub(crate) fn db(&self) -> &Connection {
        match self.db.as_ref() {
            Some(db) => db,
            None => {
                panic!("Database connection is closed!");
            }
        }
    }

    /// Convert comma separated list of participant strings into table chat IDs using
    ///   1) filter `self.participant` keys based on the values (by comparing to user values)
    ///   2) get the chat IDs keys from `self.chatroom_participants` for values that contain the selected `handle_ids`
    ///   3) send those chat and handle IDs to the query context so they are included in the message table filters
    pub(crate) fn resolve_filtered_handles(&mut self) {
        if let Some(conversation_filter) = &self.options.conversation_filter {
            let parsed_handle_filter = conversation_filter.split(',').collect::<Vec<&str>>();

            let mut included_chatrooms: BTreeSet<i32> = BTreeSet::new();
            let mut included_handles: BTreeSet<i32> = BTreeSet::new();

            // First: Scan the list of participants for included handle IDs
            self.participants
                .iter()
                .for_each(|(handle_id, handle_name)| {
                    for included_name in &parsed_handle_filter {
                        if handle_name.contains(included_name) {
                            included_handles.insert(*handle_id);
                        }
                    }
                });

            // Second, scan the list of chatrooms for IDs that contain the selected participants
            self.chatroom_participants
                .iter()
                .for_each(|(chat_id, participants)| {
                    if !participants.is_disjoint(&included_handles) {
                        included_chatrooms.insert(*chat_id);
                    }
                });

            self.options
                .query_context
                .set_selected_handle_ids(included_handles);

            self.options
                .query_context
                .set_selected_chat_ids(included_chatrooms);

            self.log_filtered_handles_and_chats();
        }
    }

    /// If we set some filtered chatrooms, emit how many will be included in the export
    fn log_filtered_handles_and_chats(&self) {
        if let (Some(selected_handle_ids), Some(selected_chat_ids)) = (
            &self.options.query_context.selected_handle_ids,
            &self.options.query_context.selected_chat_ids,
        ) {
            let unique_handle_ids: HashSet<Option<&i32>> = selected_handle_ids
                .iter()
                .map(|handle_id| self.real_participants.get(handle_id))
                .collect();

            let mut unique_chat_ids: HashSet<String> = HashSet::new();
            for selected_chat_id in selected_chat_ids {
                if let Some(participants) = self.chatroom_participants.get(selected_chat_id) {
                    unique_chat_ids.insert(self.filename_from_participants(participants));
                }
            }

            eprintln!(
                "Filtering for {} handle{} across {} chatrooms...",
                unique_handle_ids.len(),
                if unique_handle_ids.len() == 1 {
                    ""
                } else {
                    "s"
                },
                unique_chat_ids.len()
            );
        }
    }

    /// Start the app given the provided set of options. This will either run
    /// diagnostic tests on the database or export data to the specified file type.
    ///
    // # Example:
    ///
    /// ```
    /// use crate::app::{
    ///    options::{from_command_line, Options},
    ///    runtime::Config,
    /// };
    ///
    /// let args = from_command_line();
    /// let options = Options::from_args(&args);
    /// let app = Config::new(options).unwrap();
    /// app.start();
    /// ```
    pub fn start(&self) -> Result<(), RuntimeError> {
        // Ensure that if we want to filter on things, we have stuff to filter for
        if let Some(filters) = &self.options.conversation_filter {
            if !self.options.query_context.has_filters() {
                return Err(RuntimeError::InvalidOptions(format!(
                    "Selected filter `{filters}` does not match any participants!"
                )));
            }
        }

        // Ensure the path we want to export to exists
        create_dir_all(&self.options.export_path)?;
        if self.tmp_attachment_path().is_dir() {
            remove_dir_all(&self.tmp_attachment_path())?;
        } else if self.tmp_attachment_path().exists() {
            eprintln!(
                "{:?} exists and is not a directory. Aborting.",
                &self.tmp_attachment_path()
            );
        }
        create_dir_all(&self.attachment_path())?;
        create_dir_all(&self.tmp_attachment_path())?;

        let mut last_messages: HashMap<i32, (Message, Vec<PathBuf>)> = HashMap::new();
        let mut min_attachment_number: i32 = self.find_min_attachment_number(0)?;
        let logfile_path = self.options.export_path.join("LOGFILE.html");
        let mut outfile = OpenOptions::new()
            .write(true)
            .append(true)
            .create(true)
            .open(&logfile_path)?;
            
        println!("🔍 Starting iMessage deletion monitor...");
        println!("📁 Deleted messages will be saved to: {:?}", logfile_path);
        println!("📂 Attachments will be saved to: {:?}", self.attachment_path());
        println!("👀 Monitoring messages for phone number filter...");
        println!("⏱️  Scanning every few seconds for changes...\n");
        
        let mut txt_instance = TXT::new(self)?;
        // let mut scan_count = 0;
        loop {
            // scan_count += 1;
            // if scan_count % 10 == 0 {
            //     println!("💫 Scan #{} - Still monitoring for deleted messages...", scan_count);
            // }
            let new_messages = txt_instance.iter_messages()?; // TODO: Filter out messages from self
            let mut new_messages_with_attachments: HashMap<i32, (Message, Vec<PathBuf>)> =
                HashMap::new();

            for (msg_id, mut new_message) in new_messages {
                let _ = new_message.generate_text(self.db());
                let attachments = Attachment::from_message(self.db(), &new_message)?;
                let mut attachment_destinations: Vec<PathBuf> = Vec::new();

                // Detect deleted messages
                if let Some((last_message, last_message_attachments)) =
                    last_messages.remove(&msg_id)
                {
                    if new_message.is_fully_unsent() && !last_message.is_fully_unsent() {
                        self.handle_deleted_message(
                            &last_message,
                            &last_message_attachments,
                            &mut outfile,
                            &txt_instance,
                        )?;
                    }
                    attachment_destinations = last_message_attachments;
                } else {
                    // Completely new message
                    if new_message.has_attachments() {
                        self.save_attachments_locally(
                            &new_message,
                            attachments,
                            &mut min_attachment_number,
                            &mut attachment_destinations,
                        )?;
                    }
                }
                new_messages_with_attachments
                    .insert(msg_id.clone(), (new_message, attachment_destinations));
            }
            // See what old messages no longer exist, and remove any temporary attachments!
            for (msg_id, (_, attachments)) in last_messages {
                self.handle_untracked_message(msg_id, &attachments);
            }

            last_messages = new_messages_with_attachments;
            thread::sleep(Duration::from_millis(500));
        }
    }

    pub fn find_min_attachment_number(&self, start: i32) -> Result<i32, RuntimeError> {
        let mut n = start;
        while self.attachment_path().join(n.to_string()).try_exists()? {
            n += 1;
        }
        return Ok(n);
    }

    pub fn save_attachments_locally(
        &self,
        message: &Message,
        mut attachments: Vec<Attachment>,
        min_attachment_number: &mut i32,
        attachment_destinations: &mut Vec<PathBuf>,
    ) -> Result<(), RuntimeError> {
        // Save the attachments as they come in!
        attachments.iter_mut().for_each(|mut attachment| {
            let attachment_basename = min_attachment_number.to_string();
            self.options
                .attachment_manager
                .handle_attachment(message, &mut attachment, &attachment_basename, self)
                .ok_or(attachment.filename().ok_or(ATTACHMENT_NO_FILENAME))
                .unwrap();

            if let Some(p) = &attachment.copied_path {
                attachment_destinations.push(p.to_owned());
            }
            *min_attachment_number = self
                .find_min_attachment_number(*min_attachment_number + 1)
                .unwrap();
        });
        Ok(())
    }

    pub fn handle_deleted_message(
        &self,
        last_message: &Message,
        last_message_attachments: &Vec<PathBuf>,
        outfile: &mut File,
        txt_instance: &TXT,
    ) -> Result<(), RuntimeError> {
        let message_preview = last_message.text.clone()
            .unwrap_or_default()
            .chars()
            .take(50)
            .collect::<String>();
        let message_preview = if message_preview.len() >= 50 {
            format!("{}...", message_preview)
        } else {
            message_preview
        };
        
        println!(
            "🚨 DELETED MESSAGE DETECTED! \"{}\" with {} attachment(s)",
            if message_preview.is_empty() { "[No text content]" } else { &message_preview },
            last_message.num_attachments,
        );
        
        let sender = self.who(
            last_message.handle_id,
            last_message.is_from_me(),
            &last_message.destination_caller_id,
        );
        println!("   👤 From: {}", sender);
        writeln!(
            outfile,
            "<h2>==={}:{}</h2>",
            sender,
            txt_instance.get_time(last_message)
        )?;
        if let Some(text) = &last_message.text {
            if text != " " {
                writeln!(outfile, "<p>Text: {}</p><br>", text)?;
            }
        }
        writeln!(outfile, "<p>Attachments:</p><br>")?;
        for attachment in last_message_attachments {
            let mut attachment_path = self.attachment_path().canonicalize().unwrap();
            attachment_path.push(attachment.file_name().unwrap());
            println!("Renaming {:?} to {:?}", &attachment, &attachment_path);
            rename(&attachment, &attachment_path)?;
            writeln!(
                outfile,
                "<img src=\"{}\" style='width:300px'><br>",
                attachment_path
                    .into_os_string()
                    .into_string()
                    .unwrap_or("?".to_string())
            )?;
        }
        Ok(())
    }

    pub fn handle_untracked_message(&self, msg_id: i32, attachments: &Vec<PathBuf>) {
        println!("New message was sent with ID: {}", msg_id);
        if !attachments.is_empty() {
            println!("   🗑️  Cleaning up {} temporary attachment(s)", attachments.len());
        }
        attachments.iter().for_each(|attachment| {
            if attachment.exists() {
                fs::remove_file(&attachment).expect(&format!(
                    "Attachment path {:?} is a valid path",
                    &attachment
                ));
            }
        })
    }

    /// Determine who sent a message
    pub fn who<'a, 'b: 'a>(
        &'a self,
        handle_id: Option<i32>,
        is_from_me: bool,
        destination_caller_id: &'b Option<String>,
    ) -> &'a str {
        if is_from_me {
            if self.options.use_caller_id {
                return destination_caller_id.as_deref().unwrap_or(ME);
            }
            return self.options.custom_name.as_deref().unwrap_or(ME);
        } else if let Some(handle_id) = handle_id {
            return match self.participants.get(&handle_id) {
                Some(contact) => contact,
                None => UNKNOWN,
            };
        }
        UNKNOWN
    }
}

#[cfg(test)]
impl Config {
    pub fn fake_app(options: Options) -> Config {
        let connection = get_connection(&options.db_path).unwrap();
        Config {
            chatrooms: HashMap::new(),
            real_chatrooms: HashMap::new(),
            chatroom_participants: HashMap::new(),
            participants: HashMap::new(),
            real_participants: HashMap::new(),
            tapbacks: HashMap::new(),
            options,
            offset: get_offset(),
            db: Some(connection),
            backup: None,
        }
    }

    pub fn fake_message() -> Message {
        Message {
            rowid: i32::default(),
            guid: String::default(),
            text: None,
            service: Some("iMessage".to_string()),
            handle_id: Some(i32::default()),
            destination_caller_id: None,
            subject: None,
            date: i64::default(),
            date_read: i64::default(),
            date_delivered: i64::default(),
            is_from_me: false,
            is_read: false,
            item_type: 0,
            other_handle: None,
            share_status: false,
            share_direction: None,
            group_title: None,
            group_action_type: 0,
            associated_message_guid: None,
            associated_message_type: Some(i32::default()),
            balloon_bundle_id: None,
            expressive_send_style_id: None,
            thread_originator_guid: None,
            thread_originator_part: None,
            date_edited: 0,
            associated_message_emoji: None,
            chat_id: None,
            num_attachments: 0,
            deleted_from: None,
            num_replies: 0,
            components: None,
            edited_parts: None,
        }
    }

    pub(crate) fn fake_attachment() -> Attachment {
        Attachment {
            rowid: 0,
            filename: Some("a/b/c/d.jpg".to_string()),
            uti: Some("public.png".to_string()),
            mime_type: Some("image/png".to_string()),
            transfer_name: Some("d.jpg".to_string()),
            total_bytes: 100,
            is_sticker: false,
            hide_attachment: 0,
            emoji_description: None,
            copied_path: None,
        }
    }
}

impl Drop for Config {
    fn drop(&mut self) {
        if let Some(backup) = &self.backup {
            // Remove the temporary `sms.db` file if it was created
            if backup.manifest_db.is_temporary {
                if let Some(conn) = self.db.take() {
                    let path = conn.path().unwrap().to_string();
                    conn.close().ok();

                    // Remove the file, ignoring errors if any
                    if let Err(e) = remove_file(&path) {
                        eprintln!(
                            "warning: failed to remove temporary messages database at {path}: {e}"
                        );
                    }
                }
            }
        }
    }
}
